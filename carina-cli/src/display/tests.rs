use super::*;

use crate::output_style::Colorize;

use crate::plan_summary::PlanChangeSummary;
use carina_core::effect::{DeferredReplaceDelete, DeferredReplacePayload, Effect, NonEmptyDeletes};
use carina_core::plan::Plan;
use carina_core::resource::{
    AccessMissPolicy, ConcreteValue, DeferredValue, Directives, InterpolationPart, LoopSlot,
    ModuleSource, PathSegment, ResolvedDependencies, ResolvedResource, ResolvedResourceId,
    Resource, ResourceId, State, Value, apply_postfix_access,
};
use carina_core::value::{PrettyLayout, PrettyValue, render_value};

fn absolute_binding(name: &str) -> carina_core::resource::AbsoluteBindingAddress {
    carina_core::resource::absolute_binding_address_for_test(name)
}

fn root_resource_ref(binding: &str, attribute: &str) -> Value {
    Value::resource_ref(binding, attribute, Vec::new()).resolve_root_for_test()
}

fn concrete(value: ConcreteValue) -> Value {
    Value::Concrete(value)
}

#[allow(clippy::too_many_arguments)]
fn format_plan_for_test<S, D: HistoricalDisplaySource<S> + ?Sized>(
    color_mode: crate::output_style::ColorMode,
    plan: &Plan<S>,
    summary: &PlanChangeSummary,
    detail: DetailLevel,
    historical_display: &D,
    schemas: Option<&SchemaRegistry>,
    moved_origins: &HashMap<ResourceId, ResourceId>,
    prev_explicit: Option<&HashMap<ResourceId, carina_core::explicit::ExplicitFields>>,
    expansion_trace: Option<&carina_core::resource::ExpansionTrace>,
) -> String {
    let provider = crate::fixture_plan::provider_router_for_plan(
        &crate::test_support::TestCommandRuntime::new(),
        plan,
    )
    .expect("fixture mock inputs must be valid");
    format_plan_with_delete_instances(
        color_mode,
        plan,
        summary,
        detail,
        historical_display,
        schemas,
        moved_origins,
        prev_explicit,
        expansion_trace,
        &provider,
        carina_core::detail_rows::DetailComparisons::Planning(
            &carina_core::references::prepare_empty_for_tests(),
        ),
    )
    .expect("display fixture must have an initialized provider route")
}

fn for_value_access(fields: &[&str]) -> Value<LoopSlot> {
    fields.iter().fold(
        Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
        |base, field| {
            apply_postfix_access(
                base,
                PathSegment::Field {
                    name: (*field).to_string(),
                },
                0,
                AccessMissPolicy::Error,
            )
            .expect("unknown access remains deferred")
        },
    )
}

trait IntoResolvedTestResource {
    fn into_resolved_test_resource(self) -> ResolvedResource;
}
impl IntoResolvedTestResource for Resource {
    fn into_resolved_test_resource(self) -> ResolvedResource {
        ResolvedResource::new(self.into_value_snapshot())
    }
}
impl IntoResolvedTestResource for ResolvedResource {
    fn into_resolved_test_resource(self) -> ResolvedResource {
        self
    }
}
fn resolved(resource: impl IntoResolvedTestResource) -> ResolvedResource {
    resource.into_resolved_test_resource()
}
fn test_resource_ref(
    resource: &Resource,
) -> carina_core::parser::ResourceRef<
    '_,
    ModuleSource,
    carina_core::resource::NoPlaceholder,
    LoopSlot,
> {
    carina_core::parser::ResourceRef::<
        _,
        carina_core::resource::NoPlaceholder,
        carina_core::resource::LoopSlot,
    >::Resource(resource)
}
fn make_resource(
    resource_type: &str,
    name: &'static str,
    binding: &str,
    deps: &[&ResolvedResource],
) -> ResolvedResource {
    let mut resource =
        Resource::new(resource_type, name).with_absolute_binding(absolute_binding(binding));
    for dependency in deps {
        let binding = dependency
            .binding
            .as_ref()
            .map(|binding| binding.as_str())
            .unwrap_or_else(|| dependency.identity_str());
        resource.set_attr(format!("ref_{binding}"), root_resource_ref(binding, "id"));
    }
    ResolvedResource::from_resolved_parts(
        resource.into_value_snapshot(),
        deps.iter()
            .map(|dependency| dependency.resolved_id())
            .collect(),
        Default::default(),
    )
}

/// Test that print_plan does not panic when a resource has a dependency
/// on a binding that is not present in the plan (external dependency).
/// This exercises the dependency graph construction code path where
/// `.unwrap()` could theoretically panic if `dep_idx` were invalid.
#[test]
fn test_print_plan_with_external_dependency_does_not_panic() {
    let color_mode = crate::output_style::ColorMode::Plain;
    // Resource "b" depends on "a", but "a" is NOT in the plan.
    // This simulates an external/unresolved dependency.
    let a = make_resource("test.resource", "a", "a", &[]);
    let b = make_resource("test.resource", "b", "b", &[&a]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(b)));
    let provider = crate::fixture_plan::provider_router_for_plan(
        &crate::test_support::TestCommandRuntime::new(),
        &plan,
    )
    .expect("fixture mock inputs must be valid");

    // Should not panic
    print_plan(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        None,
        None,
        &provider,
        carina_core::detail_rows::DetailComparisons::Planning(
            &carina_core::references::prepare_empty_for_tests(),
        ),
    )
    .expect("display fixture must have an initialized provider route");
}

#[test]
fn apply_plan_displays_missing_persisted_destroy_dependency_warning() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let id = ResolvedResourceId::new(ResourceId::with_identity("test.resource", "orphan"));
    let missing = ResolvedResourceId::new(ResourceId::with_identity("test.resource", "vanished"));
    let generation = carina_core::effect::EffectGeneration::Current;
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: id.clone(),
        identifier: carina_core::provider::AppliedIdentifier::new("orphan-provider-id")
            .expect("fixture provider identifier"),
        generation: generation.clone(),
        directives: Default::default(),
        binding: Some(absolute_binding("orphan")),
        dependencies: crate::test_support::persisted([missing.clone()]),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    });
    plan.record_unresolvable_destroy_dependencies(
        id,
        generation,
        std::iter::once(missing.clone()).collect(),
    );

    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        None,
        None,
    ));

    assert!(output.contains("destroy ordering for test.resource.orphan is partially unknown"));
    assert!(output.contains(&format!(
        "persisted dependency `{missing}` no longer resolves"
    )));
}

/// Test that print_plan handles the dependency graph correctly when
/// dependents map is accessed with valid indices.
#[test]
fn test_print_plan_with_internal_dependency_does_not_panic() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let a = make_resource("test.resource", "a", "a", &[]);
    let b = make_resource("test.resource", "b", "b", &[&a]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(a)));
    plan.add(Effect::Create(resolved(b)));
    let provider = crate::fixture_plan::provider_router_for_plan(
        &crate::test_support::TestCommandRuntime::new(),
        &plan,
    )
    .expect("fixture mock inputs must be valid");

    // Should not panic
    print_plan(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        None,
        None,
        &provider,
        carina_core::detail_rows::DetailComparisons::Planning(
            &carina_core::references::prepare_empty_for_tests(),
        ),
    )
    .expect("display fixture must have an initialized provider route");
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
    let rt_b = make_resource("ec2.RouteTable", "rt_b", "rt_b", &[&vpc]);
    let subnet_b = make_resource("ec2.Subnet", "subnet_b", "subnet_b", &[&vpc]);
    let rt_a = make_resource("ec2.RouteTable", "rt_a", "rt_a", &[&vpc]);
    let subnet_a = make_resource("ec2.Subnet", "subnet_a", "subnet_a", &[&vpc]);

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
    let sg = make_resource("ec2.SecurityGroup", "sg", "sg", &[&vpc]);
    let endpoint = make_resource("ec2.vpc_endpoint", "endpoint", "endpoint", &[&vpc, &sg]);

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
    let rt = make_resource("ec2.RouteTable", "rt", "rt", &[&vpc]);
    let igw = make_resource("ec2.internet_gateway", "igw", "igw", &[]);
    let route = make_resource("ec2.route", "route", "route", &[&rt, &igw]);
    let igw_attachment = make_resource(
        "ec2.vpc_gateway_attachment",
        "igw_attachment",
        "igw_attachment",
        &[&vpc, &igw],
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
    let rt = make_resource("ec2.RouteTable", "rt", "rt", &[&vpc]);
    let igw = make_resource("ec2.internet_gateway", "igw", "igw", &[]);
    let route = make_resource("ec2.route", "route", "route", &[&rt, &igw]);
    let igw_attachment = make_resource(
        "ec2.vpc_gateway_attachment",
        "igw_attachment",
        "igw_attachment",
        &[&vpc, &igw],
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
    let mut r: Resource = Resource::new("ec2.subnet_route_table_association", "hash123");
    r.set_attr(
        "route_table_id".to_string(),
        root_resource_ref("public_rt", "id"),
    );
    r.set_attr(
        "subnet_id".to_string(),
        root_resource_ref("public_subnet_1a", "id"),
    );

    let hint = extract_compact_hint(test_resource_ref(&r), None);
    // Should return only the first ResourceRef alphabetically, with _id suffix stripped
    assert_eq!(hint, Some("route_table: public_rt".to_string()));
}

/// Test that extract_compact_hint skips ResourceRef that matches parent binding.
#[test]
fn test_extract_compact_hint_skips_parent_ref() {
    let mut r: Resource = Resource::new("ec2.subnet_route_table_association", "hash123");
    r.set_attr(
        "route_table_id".to_string(),
        root_resource_ref("database_rt", "id"),
    );
    r.set_attr(
        "subnet_id".to_string(),
        root_resource_ref("database_subnet_1a", "id"),
    );

    // When parent is database_rt, should skip route_table_id and show only subnet_id
    let hint = extract_compact_hint(test_resource_ref(&r), Some("database_rt"));
    assert_eq!(hint, Some("subnet: database_subnet_1a".to_string()));
}

/// Test that extract_compact_hint falls back to string values when no ResourceRef.
#[test]
fn test_extract_compact_hint_string_fallback() {
    let mut r: Resource = Resource::new("ec2.route", "hash456");
    r.set_attr(
        "destination_cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
    );

    let hint = extract_compact_hint(test_resource_ref(&r), None);
    assert_eq!(hint, Some("destination_cidr_block: 0.0.0.0/0".to_string()));
}

/// Test that extract_compact_hint returns None when no useful attributes.
#[test]
fn test_extract_compact_hint_none() {
    let r: Resource = Resource::new("ec2.route", "hash789");
    let hint = extract_compact_hint(test_resource_ref(&r), None);
    assert_eq!(hint, None);
}

/// Test that extract_compact_hint prefers string over ResourceRef.
#[test]
fn test_extract_compact_hint_prefers_string_over_resource_ref() {
    let mut r: Resource = Resource::new("ec2.route", "hash_mixed");
    r.set_attr(
        "destination".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/8".into())),
    );
    r.set_attr("gateway_id".to_string(), root_resource_ref("igw", "id"));

    let hint = extract_compact_hint(test_resource_ref(&r), None);
    // String takes priority over ResourceRef
    assert_eq!(hint, Some("destination: 10.0.0.0/8".to_string()));
}

/// Test that extract_compact_hint shortens service_name values.
#[test]
fn test_extract_compact_hint_service_name_shortening() {
    let mut r: Resource = Resource::new("ec2.vpc_endpoint", "hash_svc");
    r.set_attr(
        "service_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "com.amazonaws.ap-northeast-1.ecr.dkr".into(),
        )),
    );

    let hint = extract_compact_hint(test_resource_ref(&r), None);
    assert_eq!(hint, Some("service: ecr.dkr".to_string()));

    // Single service component
    let mut r2: Resource = Resource::new("ec2.vpc_endpoint", "hash_svc2");
    r2.set_attr(
        "service_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "com.amazonaws.ap-northeast-1.s3".into(),
        )),
    );

    let hint2 = extract_compact_hint(test_resource_ref(&r2), None);
    assert_eq!(hint2, Some("service: s3".to_string()));
}

/// Test that extract_compact_hint falls back to string when all ResourceRefs match parent.
#[test]
fn test_extract_compact_hint_all_refs_match_parent() {
    let mut r: Resource = Resource::new("ec2.security_group_ingress", "hash_sg");
    r.set_attr(
        "group_id".to_string(),
        root_resource_ref("endpoint_sg", "id"),
    );
    r.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("Allow HTTPS from VPC".into())),
    );

    // When parent is endpoint_sg, should skip group_id and use description
    let hint = extract_compact_hint(test_resource_ref(&r), Some("endpoint_sg"));
    assert_eq!(hint, Some("description: Allow HTTPS from VPC".to_string()));
}

/// Test that extract_compact_hint prefers string over ResourceRef for vpc_endpoint.
#[test]
fn test_extract_compact_hint_prefers_string_for_vpc_endpoint() {
    let mut r: Resource = Resource::new("ec2.vpc_endpoint", "hash_ep");
    r.set_attr(
        "service_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "com.amazonaws.ap-northeast-1.ecr.dkr".into(),
        )),
    );
    r.set_attr(
        "security_group_ids".to_string(),
        Value::Concrete(ConcreteValue::List(vec![root_resource_ref(
            "endpoint_sg",
            "group_id",
        )])),
    );
    r.set_attr("vpc_id".to_string(), root_resource_ref("vpc", "id"));

    // String attribute (service_name) takes priority over ResourceRef
    let hint = extract_compact_hint(test_resource_ref(&r), Some("vpc"));
    assert_eq!(hint, Some("service: ecr.dkr".to_string()));
}

/// Test that has_binding correctly detects bound vs anonymous resources.
#[test]
fn test_has_binding() {
    let mut bound: Resource = Resource::new("ec2.Vpc", "vpc");
    bound.binding = Some(absolute_binding("vpc"));
    assert!(has_binding(carina_core::parser::ResourceRef::<
        _,
        carina_core::resource::NoPlaceholder,
        carina_core::resource::LoopSlot,
    >::Resource(&bound)));

    let anonymous: Resource = Resource::new("ec2.Vpc", "hash123");
    assert!(!has_binding(carina_core::parser::ResourceRef::<
        _,
        carina_core::resource::NoPlaceholder,
        carina_core::resource::LoopSlot,
    >::Resource(&anonymous)));
}

/// Test that format_compact_name shows plain identifiers for bound resources and
/// parenthesized hints for anonymous resources.
#[test]
fn test_format_compact_name_bound_resource() {
    let mut r: Resource = Resource::new("ec2.Vpc", "vpc");
    r.binding = Some(absolute_binding("vpc"));
    // For bound resources, should show name as plain identifier (no quotes)
    let result = format_compact_name(test_resource_ref(&r), "vpc", None);
    assert!(
        !result.contains('"'),
        "Bound resource name should not be quoted"
    );
    assert_eq!(result, "vpc", "Should be the plain binding name");
}

#[test]
fn test_format_compact_name_anonymous_with_hint() {
    let mut r: Resource = Resource::new("ec2.subnet_route_table_association", "hash123");
    r.set_attr(
        "subnet_id".to_string(),
        root_resource_ref("database_subnet_1a", "id"),
    );
    let result = format_compact_name(test_resource_ref(&r), "hash123", None);
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
    let color_mode = crate::output_style::ColorMode::Plain;
    let vpc = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let rt = make_resource("ec2.RouteTable", "rt", "rt", &[&vpc]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(vpc)));
    plan.add(Effect::Create(resolved(rt)));
    let provider = crate::fixture_plan::provider_router_for_plan(
        &crate::test_support::TestCommandRuntime::new(),
        &plan,
    )
    .expect("fixture mock inputs must be valid");

    // Should not panic
    print_plan(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        None,
        None,
        &provider,
        carina_core::detail_rows::DetailComparisons::Planning(
            &carina_core::references::prepare_empty_for_tests(),
        ),
    )
    .expect("display fixture must have an initialized provider route");
}

/// Test compact mode hides provider attributes for anonymous resources.
#[test]
fn test_print_plan_compact_with_anonymous_resources() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let mut anon: Resource = Resource::new("ec2.route", "hash_anon");
    anon.set_attr(
        "destination_cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
    );
    anon.set_attr(
        "route_table_id".to_string(),
        root_resource_ref("public_rt", "id"),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(anon)));
    let provider = crate::fixture_plan::provider_router_for_plan(
        &crate::test_support::TestCommandRuntime::new(),
        &plan,
    )
    .expect("fixture mock inputs must be valid");

    // Should not panic; anonymous resources should show hints
    print_plan(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        None,
        None,
        &provider,
        carina_core::detail_rows::DetailComparisons::Planning(
            &carina_core::references::prepare_empty_for_tests(),
        ),
    )
    .expect("display fixture must have an initialized provider route");
}

/// Test that extract_compact_hint extracts ResourceRef from inside a List value.
/// e.g., security_group_ids = [endpoint_sg.group_id] should produce "security_group: endpoint_sg"
#[test]
fn test_extract_compact_hint_list_containing_resource_ref() {
    let mut r: Resource = Resource::new("ec2.vpc_endpoint", "hash_list_ref");
    r.set_attr(
        "security_group_ids".to_string(),
        Value::Concrete(ConcreteValue::List(vec![root_resource_ref(
            "endpoint_sg",
            "group_id",
        )])),
    );

    let hint = extract_compact_hint(test_resource_ref(&r), None);
    assert_eq!(
        hint,
        Some("security_group: endpoint_sg".to_string()),
        "Should extract ResourceRef from inside List and strip _ids suffix"
    );
}

/// Test that extract_compact_hint skips List<ResourceRef> when ref matches parent.
#[test]
fn test_extract_compact_hint_list_ref_skips_parent() {
    let mut r: Resource = Resource::new("ec2.vpc_endpoint", "hash_list_parent");
    r.set_attr(
        "security_group_ids".to_string(),
        Value::Concrete(ConcreteValue::List(vec![root_resource_ref(
            "endpoint_sg",
            "group_id",
        )])),
    );

    let hint = extract_compact_hint(test_resource_ref(&r), Some("endpoint_sg"));
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

/// String values retain their full text even when they resemble enum identifiers.
#[test]
fn test_extract_compact_hint_preserves_namespaced_string() {
    let mut r: Resource = Resource::new("ec2.Subnet", "hash_enum");
    r.set_attr(
        "availability_zone".to_string(),
        Value::Concrete(ConcreteValue::String(
            "awscc.AvailabilityZone.ap_northeast_1a".into(),
        )),
    );

    let hint = extract_compact_hint(test_resource_ref(&r), None);
    assert_eq!(
        hint,
        Some("availability_zone: awscc.AvailabilityZone.ap_northeast_1a".to_string()),
        "string values must not be reclassified as enum identifiers"
    );
}

/// Desired-resource consumers trust the parser boundary and inspect every
/// attribute rather than carrying another internal-key filter.
#[test]
fn test_extract_compact_hint_considers_all_resource_attributes() {
    let mut r: Resource = Resource::new("ec2.Vpc", "hash_internal");
    r.binding = Some(absolute_binding("vpc"));
    r.set_attr(
        "_hash".to_string(),
        Value::Concrete(ConcreteValue::String("abc123".into())),
    );

    let hint = extract_compact_hint(test_resource_ref(&r), None);
    assert_eq!(hint, Some("_hash: abc123".to_string()));
}

#[test]
fn test_deferred_create_renders_deferred_until_apply_marker() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let mut template_resource: Resource<ModuleSource, LoopSlot> =
        Resource::with_provider("aws", "route53.Record", "validation_records", None);
    template_resource.binding = Some(absolute_binding("validation_records"));
    template_resource.set_attr("name", for_value_access(&["resource_record", "name"]));

    let deferred = carina_core::parser::DeferredForExpression::new(
        None,
        1,
        "for opt in cert.domain_validation_options".to_string(),
        carina_core::parser::DeferredForTarget::bound(
            carina_core::resource::absolute_binding_address_for_test("validation_records"),
        )
        .expect("fixture binding is valid"),
        root_resource_ref("cert", "domain_validation_options"),
        carina_core::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    );

    let mut plan = Plan::new();
    plan.add(Effect::DeferredCreate(crate::test_support::compiled_loop(
        Box::new(deferred),
    )));

    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
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

fn certificate_resource() -> Resource {
    Resource::new("acm.Certificate", "cert").with_absolute_binding(absolute_binding("cert"))
}

fn deferred_validation_records_effect() -> Effect {
    let mut template_resource: Resource<ModuleSource, LoopSlot> =
        Resource::with_provider("aws", "route53.Record", "validation_records", None);
    template_resource.binding = Some(absolute_binding("validation_records"));
    template_resource.set_attr("name", for_value_access(&["resource_record", "name"]));

    let deferred = carina_core::parser::DeferredForExpression::new(
        None,
        1,
        "for opt in cert.domain_validation_options".to_string(),
        carina_core::parser::DeferredForTarget::bound(
            carina_core::resource::absolute_binding_address_for_test("validation_records"),
        )
        .expect("fixture binding is valid"),
        root_resource_ref("cert", "domain_validation_options"),
        carina_core::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    );

    let mut configuration = carina_core::parser::ExpandedParsedFile::default();
    configuration.resources = vec![certificate_resource()];
    let graph = carina_core::resolved_graph::resolve_expanded_graph(configuration.into())
        .expect("certificate fixture graph resolves");
    let (template, dependency_template) = graph
        .compile_deferred_for(deferred)
        .expect("deferred fixture references the certificate producer");
    Effect::DeferredCreate(carina_core::effect::DeferredCreatePayload::new(
        Box::new(template),
        dependency_template,
    ))
}

#[test]
fn format_effect_bound_deferred_for_uses_the_template_identity() {
    assert_eq!(
        format_effect(&deferred_validation_records_effect()),
        "Expand deferred for aws.route53.Record validation_records[?] (waits on cert)",
    );
}

#[test]
fn format_effect_unbound_deferred_for_uses_the_source_identity() {
    let source = "let upstream = mock.test.resource { name = \"upstream\" }\n\
        for value in upstream.tags { mock.test.resource { name = value } }\n";
    let parsed = carina_core::parser::parse_file(
        source,
        std::path::Path::new("deferred-progress.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("unbound loop parses");
    let parsed = carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |session| {
            carina_core::module_resolver::resolve_modules_providerless(
                carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                std::path::Path::new("."),
                session,
            )
            .expect("root bindings resolve")
        },
    )
    .into_expanded(&carina_core::schema::SchemaRegistry::new())
    .expect("loop slots finalize");
    let deferred = parsed
        .deferred_for_expressions()
        .iter()
        .next()
        .expect("deferred loop")
        .clone();
    assert!(matches!(
        deferred.target(),
        carina_core::parser::DeferredForTarget::Unbound {
            template: _template
        }
    ));
    let effect = Effect::DeferredCreate(crate::test_support::compiled_loop(Box::new(deferred)));
    assert_eq!(
        format_effect(&effect),
        "Expand deferred for mock.test.resource _loop_39ec86fd0e24c726 (waits on upstream)",
    );
}

fn delete_record_effect(binding: &'static str) -> Effect {
    Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "route53.Record",
            binding,
        )),
        identifier: crate::fixture_applied_identifier(format!("{binding}-id")),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute_binding(binding)),
        dependencies: crate::test_support::persisted([ResolvedResourceId::new(
            certificate_resource().id,
        )]),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    }
}

fn deferred_replace_validation_records_effect() -> Effect {
    let Effect::DeferredCreate(payload) = deferred_validation_records_effect() else {
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
        ..
    } = delete_record_effect("validation_records[0]")
    else {
        unreachable!("helper constructs Delete")
    };

    Effect::DeferredReplace(Box::new(DeferredReplacePayload {
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
        template: payload.template,
        dependency_template: payload.dependency_template,
    }))
}

#[test]
fn test_deferred_replace_renders_top_level() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let mut plan = Plan::new();
    plan.add(deferred_replace_validation_records_effect());

    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
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
    let color_mode = crate::output_style::ColorMode::Plain;
    let mut dependent = Resource::new("acm.CertificateValidation", "cert-validation");
    dependent.binding = Some(absolute_binding("cert_validation"));
    dependent.set_attr(
        "validation_record_id",
        root_resource_ref("validation_records", "id"),
    );

    let mut plan = Plan::new();
    let gate = deferred_replace_validation_records_effect();
    let gate_id = gate.scheduler_id();
    plan.add(gate);
    plan.add(Effect::Create(ResolvedResource::from_resolved_parts(
        resolved(dependent).into_inner(),
        ResolvedDependencies::from_iter([gate_id]),
        ResolvedDependencies::default(),
    )));

    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
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
                  validation_record_id: validation_records.id

    Plan: 1 to add, 0 to change, 0 to destroy.
           N to replace after cert resolves.
    ");
}

#[test]
fn test_deferred_replace_keeps_unrelated_same_type_delete() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(certificate_resource())));
    plan.add(delete_record_effect("old_record"));
    plan.add(deferred_replace_validation_records_effect());

    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
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
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )]),
    )
    .with_identifier("vpc-id");

    // SG: Create effect with RESOLVED ref (string instead of ResourceRef).
    // This is what happens after resolve_refs_with_state() runs:
    // vpc_id = vpc.vpc_id becomes vpc_id = "vpc-0123456789abcdef0"
    let mut sg: Resource = Resource::new("ec2.SecurityGroup", "sg");
    sg.binding = Some(absolute_binding("sg"));
    // This is the resolved value — a plain string, NOT a ResourceRef
    sg.set_attr(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-0123456789abcdef0".into())),
    );
    sg.set_attr(
        "group_description".to_string(),
        Value::Concrete(ConcreteValue::String("Test security group".into())),
    );
    // _dependency_bindings is saved by resolve_refs_with_state() before
    // ResourceRef values are resolved to strings.
    sg.dependencies.value_bindings = std::collections::BTreeSet::from([absolute_binding("vpc")]);

    let sg = ResolvedResource::from_resolved_parts(
        sg.into_value_snapshot(),
        std::iter::once(vpc_to.resolved_id()).collect(),
        Default::default(),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("vpc-id"),
        from: Box::new(vpc_from),
        to: resolved(vpc_to),
        changed_attributes: vec!["tags".to_string()],
    });
    plan.add(Effect::Create(resolved(sg)));

    let (roots, dependents, effect_bindings, _effect_types) = build_plan_tree(&plan);

    // VPC (idx 0) should be the only root.
    // SG has resolved graph metadata that preserves the dependency on VPC.
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
    let effect: Effect = Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "ec2_vpc_fb75c929",
            None,
        )),
        identifier: crate::fixture_applied_identifier("vpc-12345"),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute_binding("my_vpc")),
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    };
    assert_eq!(format_effect(&effect), "Delete awscc.ec2.Vpc my_vpc");
}

#[test]
fn format_effect_delete_falls_back_to_id_name() {
    let effect: Effect = Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "ec2_vpc_fb75c929",
            None,
        )),
        identifier: crate::fixture_applied_identifier("vpc-12345"),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
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
        identifier: crate::fixture_applied_identifier("role-id"),
        from: Box::new(State::existing(id.clone(), HashMap::new()).with_identifier("role-id")),
        to: resolved(to),
        changed_attributes: Vec::new(),
    };
    assert_eq!(
        format_effect(&effect),
        "Update awscc.iam.Role bs.bootstrap.role"
    );
}

#[test]
fn format_effect_import_separates_type_and_name_with_space() {
    let effect: Effect = Effect::Import {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "aws",
            "s3.Bucket",
            "logs",
            None,
        )),
        identifier: carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("my-logs-bucket".into()),
        ),
    };
    assert_eq!(
        format_effect(&effect),
        "Import aws.s3.Bucket logs (id: my-logs-bucket)"
    );
}

#[test]
fn format_effect_remove_separates_type_and_name_with_space() {
    let effect: Effect = Effect::Remove {
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
    let effect: Effect = Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc", "ec2.Vpc", "vpc_a", None,
        )),
        to: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc", "ec2.Vpc", "vpc_b", None,
        )),
        origin: None,
    };
    assert_eq!(
        format_effect(&effect),
        "Move awscc.ec2.Vpc vpc_a -> awscc.ec2.Vpc vpc_b"
    );
}

#[test]
fn colored_value_list_elements_colored_individually() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let rendered = render_value(&concrete(ConcreteValue::List(vec![
        concrete(ConcreteValue::String("hello".into())),
        concrete(ConcreteValue::String("world".into())),
    ])));
    let result = colored_value(color_mode, &rendered);
    // Each quoted string should be green, not the whole list
    assert!(result.contains('['));
    assert!(result.contains(']'));
    // The individual strings should have color codes
    let green_hello = "'hello'".green(color_mode).to_string();
    assert!(
        result.contains(&green_hello),
        "Expected green-colored hello in: {}",
        result
    );
}

#[test]
fn colored_value_resource_reference_is_cyan_from_span_kind() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let result = colored_value(
        color_mode,
        &render_value(&root_resource_ref("binding", "attr")),
    );
    let cyan_elem = "binding.attr".cyan(color_mode).to_string();
    assert!(
        result.contains(&cyan_elem),
        "Expected cyan ref in: {}",
        result
    );
}

#[test]
fn colored_value_map_values_colored() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test-vpc".into())),
        ),
        ("count".to_string(), Value::Concrete(ConcreteValue::Int(42))),
    ])));
    let result = colored_value(color_mode, &render_value(&value));
    let green_val = "'test-vpc'".green(color_mode).to_string();
    let white_val = "42".white(color_mode).to_string();
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
fn colored_value_map_key_apostrophe_does_not_swallow_following_values() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        (
            "it's".to_string(),
            Value::Concrete(ConcreteValue::String("first".into())),
        ),
        (
            "later".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
    ])));

    let result = colored_value(color_mode, &render_value(&value));
    assert!(result.contains(r#""it's": "#));
    assert!(result.contains(&"'first'".green(color_mode).to_string()));
    assert!(result.contains(&"true".yellow(color_mode).to_string()));
}

#[test]
fn colored_value_keeps_interpolation_with_quoted_argument_as_one_list_element() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let interpolation: Value =
        Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
            Value::Deferred(DeferredValue::FunctionCall {
                name: carina_core::builtins::BuiltinFunction::join,
                args: vec![
                    Value::Concrete(ConcreteValue::String("it's".into())).into(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: absolute_binding("x"),
                    })
                    .into(),
                ],
                source_line: (1).into(),
            }),
        )]));
    let value: Value = Value::Concrete(ConcreteValue::List(vec![
        interpolation,
        Value::Concrete(ConcreteValue::String("next".into())),
    ]));

    let result = colored_value(color_mode, &render_value(&value));
    assert!(result.contains(&r#""${join("it's", x)}""#.green(color_mode).to_string()));
    assert!(result.contains(&"'next'".green(color_mode).to_string()));
}

#[test]
fn colored_value_does_not_split_colon_inside_vertical_string() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let value: Value = Value::Concrete(ConcreteValue::StringList(vec![
        "a: b".into(),
        "a value long enough to force this list onto vertical lines in plan output".into(),
    ]));
    let rendered = PrettyValue::new(value).render(PrettyLayout::for_attribute(20, "values"));

    assert!(rendered.plain().contains("\n"));
    assert!(
        colored_value(color_mode, &rendered).contains(&"'a: b'".green(color_mode).to_string()),
        "the colon-bearing string must remain one green span"
    );
}

#[test]
fn colored_value_empty_list() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let value: Value = Value::Concrete(ConcreteValue::List(Vec::new()));
    assert_eq!(colored_value(color_mode, &render_value(&value)), "[]");
}

#[test]
fn colored_value_empty_map() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::new()));
    assert_eq!(colored_value(color_mode, &render_value(&value)), "{}");
}

/// Strip ANSI SGR (color) escape sequences for assertion purposes.
/// Matches the helper used by `plan_snapshot_tests.rs` and
/// `module_info_snapshot_tests.rs` (both call sites use this same regex).
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

fn assert_no_ansi_style_crosses_newline(rendered: &str) {
    let chars: Vec<char> = rendered.chars().collect();
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\u{1b}' && chars.get(i + 1) == Some(&'[') {
            let mut end = i + 2;
            while end < chars.len() && chars[end] != 'm' {
                end += 1;
            }
            let params: String = chars[i + 2..end].iter().collect();
            if params == "0" {
                depth = (depth - 1).max(0);
            } else {
                depth += 1;
            }
            i = end + 1;
            continue;
        }
        if chars[i] == '\n' {
            assert_eq!(
                depth, 0,
                "an ANSI style span crossed a newline: {rendered:?}"
            );
        }
        i += 1;
    }
}

fn multiline_list_of_maps_for_gutter_test() -> Value {
    Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("first".into())),
        )]))),
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("second".into())),
        )]))),
    ]))
}

#[test]
fn render_value_ansi_restores_gutter_without_styling_line_boundaries() {
    let color_mode = crate::output_style::ColorMode::Ansi;

    let prefix = "     │  ";
    let rendered = PrettyValue::new(multiline_list_of_maps_for_gutter_test())
        .render(PrettyLayout::for_attribute(prefix.chars().count(), "items"));
    let ansi = render_value_ansi(
        color_mode,
        &rendered,
        AnsiValueStyle::Semantic,
        Some(prefix),
    );
    let plain = strip_ansi(&ansi);

    assert!(
        ansi.contains("\u{1b}[32m"),
        "fixture must exercise ANSI: {ansi:?}"
    );
    assert!(
        plain
            .lines()
            .skip(1)
            .all(|line| line.starts_with(prefix.trim_end())),
        "every continuation line must retain the gutter: {plain:?}"
    );
    assert!(
        plain.lines().any(|line| line == prefix.trim_end()),
        "the inter-element blank line must contain only the bare gutter: {plain:?}"
    );
    assert_no_ansi_style_crosses_newline(&ansi);
    for line in ansi.lines().skip(1) {
        let plain_line = strip_ansi(line);
        if plain_line == prefix.trim_end() {
            assert_eq!(line, prefix.trim_end(), "the bare gutter must be unstyled");
            continue;
        }
        let tail = plain_line
            .strip_prefix(prefix)
            .expect("non-blank continuation lines retain the full gutter prefix");
        let extra_indent = tail.bytes().take_while(|byte| *byte == b' ').count();
        let unstyled_prefix = format!("{prefix}{}", &tail[..extra_indent]);
        assert!(
            line.starts_with(&unstyled_prefix),
            "ANSI must not precede the continuation indentation or gutter: {line:?}"
        );
    }
}

#[test]
fn render_value_ansi_single_line_with_gutter_is_unchanged() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let rendered = render_value(&concrete(ConcreteValue::List(Vec::new())));
    assert_eq!(
        render_value_ansi(
            color_mode,
            &rendered,
            AnsiValueStyle::Semantic,
            Some("     │  ")
        ),
        "[]"
    );
}

#[test]
fn render_value_ansi_space_only_prefix_preserves_pretty_layout() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let prefix = "      ";
    let rendered = PrettyValue::new(multiline_list_of_maps_for_gutter_test())
        .render(PrettyLayout::for_attribute(prefix.chars().count(), "items"));
    let output = render_value_ansi(
        color_mode,
        &rendered,
        AnsiValueStyle::Semantic,
        Some(prefix),
    );
    assert_eq!(strip_ansi(&output), rendered.plain());
}

#[test]
fn colored_value_preserves_vertical_list_layout() {
    let color_mode = crate::output_style::ColorMode::Ansi;

    // `PrettyValue::render` emits a multi-line bracketed form for lists that
    // exceed the 80-col threshold. `colored_value` must NOT collapse the
    // layout back to inline — it should color atoms in place and keep
    // newlines / leading indentation intact.
    let value: Value = Value::Concrete(ConcreteValue::StringList(vec![
        "a".repeat(40).into(),
        "b".repeat(40).into(),
        "c".repeat(40).into(),
    ]));
    let rendered = PrettyValue::new(value).render(PrettyLayout::for_attribute(0, "values"));
    let input = rendered.plain();
    let result = colored_value(color_mode, &rendered);
    let stripped = strip_ansi(&result);
    assert_eq!(
        stripped, input,
        "vertical layout (newlines, indent, trailing comma, closing bracket) must be preserved verbatim",
    );
    let green_a = format!("'{}'", "a".repeat(40))
        .green(color_mode)
        .to_string();
    assert!(
        result.contains(&green_a),
        "string atoms inside the vertical list should still be colored: {result:?}",
    );
}

#[test]
fn colored_value_preserves_vertical_list_closing_bracket_alone() {
    let color_mode = crate::output_style::ColorMode::Plain;
    // The closing `]` sits on its own line, indented to the parent. This is
    // the most layout-fragile token — earlier impls collapsed it into the
    // previous line as `,]`. Pin its standalone-line preservation explicitly.
    let value: Value = Value::Concrete(ConcreteValue::StringList(vec!["x".repeat(80).into()]));
    let rendered = PrettyValue::new(value).render(PrettyLayout::for_attribute(0, "values"));
    let input = rendered.plain();
    let result = colored_value(color_mode, &rendered);
    let stripped = strip_ansi(&result);
    assert_eq!(stripped, input);
    assert!(
        stripped.ends_with("\n]"),
        "closing bracket must remain on its own line, got: {stripped:?}",
    );
}

#[test]
fn colored_value_preserves_vertical_map_layout() {
    let color_mode = crate::output_style::ColorMode::Plain;
    // Same constraint for `format_map_vertical` output.
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        (
            "k1".to_string(),
            Value::Concrete(ConcreteValue::String("v1".into())),
        ),
        ("k2".to_string(), Value::Concrete(ConcreteValue::Int(42))),
    ])));
    let rendered = PrettyValue::new(value).render(PrettyLayout::for_attribute(75, "value"));
    let input = rendered.plain();
    let result = colored_value(color_mode, &rendered);
    let stripped = strip_ansi(&result);
    assert_eq!(
        stripped, input,
        "vertical map layout must be preserved verbatim, got: {result:?}",
    );
}

#[test]
fn plan_export_duration_renders_canonical() {
    let v: Value = Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60)));
    assert_eq!(render_value(&v).plain(), "1min");
}

#[test]
fn plan_export_values_use_the_core_value_formatter() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::ExportChange;
    use carina_core::resource::RawEnumIdentifier;

    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        (
            "ratio".to_string(),
            Value::Concrete(ConcreteValue::Float(1.0)),
        ),
        (
            "region".to_string(),
            Value::Concrete(ConcreteValue::EnumIdentifier(RawEnumIdentifier::parse(
                "aws.Region.ap_northeast_1",
            ))),
        ),
    ])));
    let change = ExportChange::Added {
        name: "settings".to_string(),
        type_expr: None,
        new_value: value,
    };

    assert_eq!(
        strip_ansi(&format_export_change(color_mode, &change)),
        "  + settings = {ratio: 1.0, region: aws.Region.ap_northeast_1}\n"
    );
}

#[test]
fn plan_summary_carries_the_exact_export_changes_rendered_by_text_output() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::ExportChange;

    let plan = Plan::new();
    let export_changes = vec![ExportChange::Added {
        name: "region".to_string(),
        type_expr: None,
        new_value: Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    }];
    let summary = PlanChangeSummary::new(
        &plan,
        &export_changes,
        &crate::plan_summary::resolved_deferred_for_for_test([]),
        &[],
    );
    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &summary,
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        None,
        None,
    ));

    assert!(
        output.contains("Exports:"),
        "missing export section:\n{output}"
    );
    assert!(
        output.contains("+ region = 'ap-northeast-1'"),
        "missing export value:\n{output}"
    );
}

#[test]
fn persisted_plan_export_maps_use_the_core_value_formatter() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::ExportChange;

    let change = ExportChange::Removed {
        name: "settings".to_string(),
        old_json: serde_json::from_value(serde_json::json!({
            "ratio": 1.0
        }))
        .unwrap(),
    };

    assert_eq!(
        strip_ansi(&format_export_change(color_mode, &change)),
        "  - settings = {ratio: 1.0}\n"
    );
}

#[test]
fn modified_plan_export_formats_old_and_new_floats_identically() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::ExportChange;

    for (value, expected) in [
        (1e21, "1000000000000000000000.0"),
        (1e-7, "0.0000001"),
        (1.0, "1.0"),
    ] {
        let change = ExportChange::Modified {
            name: "ratio".to_string(),
            type_expr: None,
            old_json: serde_json::from_value(serde_json::json!(value)).unwrap(),
            new_value: Value::Concrete(ConcreteValue::Float(value)),
        };

        assert_eq!(
            strip_ansi(&format_export_change(color_mode, &change)),
            format!("  ~ ratio = {expected} → {expected}\n")
        );
    }
}

#[test]
fn plan_export_shows_env_plaintext() {
    let name = "CARINA_TEST_ENV_EXPORT_PLAN_DISPLAY";
    unsafe {
        std::env::set_var(name, "hunter2SECRET");
    }
    let value = carina_core::builtins::evaluate_builtin_to_value(
        "env",
        &[concrete(ConcreteValue::String(name.into()))],
    )
    .expect("env value");

    assert_eq!(render_value(&value).plain(), "'hunter2SECRET'");

    unsafe {
        std::env::remove_var(name);
    }
}

#[test]
fn format_deferred_value_duration_renders_canonical() {
    // Same canonical formatter, with the deferred-for context applied.
    let v: Value<LoopSlot> =
        Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60)));
    let result = carina_core::plan_tree::render_deferred_for_template_value(&v, "cert", "applies");
    // The deferred path may inject ANSI dimming for Unknown variants,
    // but a resolved Duration must render verbatim.
    assert_eq!(result.plain(), "1min");
}

/// Regression for #3115: a deleted-effect attribute whose value is a
/// multi-line `PrettyValue::render` map (e.g. CloudFront
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
    let color_mode = crate::output_style::ColorMode::Ansi;
    use indexmap::IndexMap;

    // A nested map forces `PrettyValue::render` into a multi-line
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
        Value::Concrete(ConcreteValue::String("redirect-to-https".into())),
    );
    cache_behavior.insert(
        "forwarded_values".to_string(),
        Value::Concrete(ConcreteValue::Map(forwarded_values)),
    );

    let row = DetailRow::PrettyAttribute {
        key: "default_cache_behavior".to_string(),
        value: PrettyValue::new(Value::Concrete(ConcreteValue::Map(cache_behavior))),
    };
    let effect: Effect = Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(
            carina_core::resource::ResourceId::with_identity("cloudfront.Distribution", "dist"),
        ),
        identifier: crate::fixture_applied_identifier("E123"),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: Default::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    };

    // Force ANSI styling on: the test harness' stdout is not a TTY, so
    // `colored` auto-disables escapes and the bug (which is *in* the
    // escape placement) would be invisible without this override.

    let mut out = String::new();
    // Non-empty `attr_prefix` so continuation lines have a real indent.
    render_detail_row(color_mode, &mut out, &row, &effect, "    ");

    // A style that opens on one line and resets on a later line keeps
    // strikethrough/red active across the intervening newline(s), so
    // the terminal paints the strike over the leading indentation of
    // every continuation line. The typed-span renderer opens and closes
    // the style *within* each line, after that line's indentation.
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
    let color_mode = crate::output_style::ColorMode::Ansi;
    let row = DetailRow::ChangedForcesReplacement {
        key: "name".to_string(),
        old: render_value(&concrete(ConcreteValue::String("old".into()))),
        new: render_value(&concrete(ConcreteValue::String("new".into()))),
    };
    let effect = Effect::Update {
        identifier: crate::fixture_applied_identifier("w"),
        from: Box::new(
            State::existing(
                ResourceId::with_identity("test.Widget", "w"),
                HashMap::new(),
            )
            .with_identifier("w"),
        ),
        to: resolved(Resource::new("test.Widget", "w")),
        changed_attributes: vec!["name".to_string()],
    };

    let mut out = String::new();
    render_detail_row(color_mode, &mut out, &row, &effect, "  ");

    assert!(
        out.contains("\u{1b}[32m'new'\u{1b}[0m"),
        "forcing Changed rows must render the new value with plain green: {out:?}"
    );
}

#[test]
fn forcing_changed_multiline_green_style_does_not_cross_newline() {
    let color_mode = crate::output_style::ColorMode::Ansi;
    let new = PrettyValue::new(concrete(ConcreteValue::StringList(vec![
        "new".repeat(30).into(),
    ])))
    .render(PrettyLayout::for_attribute(60, "rules"));
    assert!(new.plain().contains('\n'));
    let row = DetailRow::ChangedForcesReplacement {
        key: "rules".to_string(),
        old: render_value(&concrete(ConcreteValue::List(Vec::new()))),
        new,
    };
    let effect = Effect::Update {
        identifier: crate::fixture_applied_identifier("w"),
        from: Box::new(
            State::existing(
                ResourceId::with_identity("test.Widget", "w"),
                HashMap::new(),
            )
            .with_identifier("w"),
        ),
        to: resolved(Resource::new("test.Widget", "w")),
        changed_attributes: vec!["rules".to_string()],
    };

    let mut out = String::new();
    render_detail_row(color_mode, &mut out, &row, &effect, "  ");

    assert!(
        !out.contains("\u{1b}[32m\n"),
        "green must not cross a newline: {out:?}"
    );
    assert!(
        !out.contains("\n\u{1b}[32m  "),
        "indentation must remain unstyled: {out:?}"
    );
}

#[test]
fn string_list_diff_renders_preformatted_literals_without_adding_quotes() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let row = DetailRow::StringListDiff {
        key: "items".to_string(),
        unchanged: Vec::new(),
        added: vec![render_value(&concrete(ConcreteValue::String("a".into())))],
        removed: Vec::new(),
    };
    let effect = Effect::Update {
        identifier: crate::fixture_applied_identifier("w"),
        from: Box::new(
            State::existing(
                ResourceId::with_identity("test.Widget", "w"),
                HashMap::new(),
            )
            .with_identifier("w"),
        ),
        to: resolved(Resource::new("test.Widget", "w")),
        changed_attributes: vec!["items".to_string()],
    };

    let mut out = String::new();
    render_detail_row(color_mode, &mut out, &row, &effect, "");

    assert!(
        strip_ansi(&out).contains("  + 'a'\n"),
        "rendered row: {out:?}"
    );
}

/// carina#3322: the composition group header reads
/// `▾ module "<binding>" (<source_path>)`. The keyword is `module`,
/// not the internal `LiveComposition`; the binding name is the user's
/// `let` LHS, and the parenthesized suffix surfaces the DSL `use`
/// path so the operator can trace the group back to a real `.crn`
/// file.
#[test]
fn test_composition_header_renders_module_with_source_path() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let header = strip_ansi(&format_composition_header(
        color_mode,
        "r",
        Some("./modules/infra"),
    ));
    assert_eq!(header, r#"  ▾ module "r" (./modules/infra)"#);
}

/// Fallback shape when no `use` path was recorded for the call site
/// (test fixtures, hand-built traces). The parenthesized suffix is
/// dropped so the header still reads as a clean `▾ module "<binding>"`
/// line — never as a literal "None" or an empty `()`.
#[test]
fn test_composition_header_drops_parens_for_none_source_path() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let header = strip_ansi(&format_composition_header(color_mode, "r", None));
    assert_eq!(header, r#"  ▾ module "r""#);
}

#[test]
fn module_header_sigil_is_module_specific_not_create() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let module_sigil = Sigil::module_header(color_mode);
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
    let color_mode = crate::output_style::ColorMode::Plain;
    fn raw_sigil(raw: &'static str) -> Sigil {
        let color_mode = crate::output_style::ColorMode::Plain;
        Sigil {
            raw,
            rendered: raw.normal(color_mode),
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
        ("module_header", Sigil::module_header(color_mode)),
        ("deferred_for", Sigil::deferred_for_expression(color_mode)),
        ("export_added", Sigil::export_added(color_mode)),
        ("export_modified", Sigil::export_modified(color_mode)),
        ("export_removed", Sigil::export_removed(color_mode)),
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
    let color_mode = crate::output_style::ColorMode::Plain;
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
        EphemeralId::new(carina_core::resource::ResolvedResourceId::new(
            ResourceId::with_identity("_virtual", "cluster"),
        )),
        "./modules/cluster",
    );
    let mut trace = ExpansionTrace::new();
    trace.record(PersistentId::new(inner_id), vec![cluster_site.clone()]);
    trace.record(PersistentId::new(role_id), vec![cluster_site]);

    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::None,
        &HashMap::new(),
        Some(&SchemaRegistry::new()),
        &HashMap::new(),
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
    let color_mode = crate::output_style::ColorMode::Plain;
    use carina_core::resource::{CallSite, EphemeralId, ExpansionTrace, PersistentId};
    use carina_core::schema::SchemaRegistry;

    let cluster = make_resource("aws.eks.Cluster", "cluster/inner", "inner", &[]);
    let node_role = make_resource(
        "aws.iam.Role",
        "cluster/node-role",
        "node_role",
        &[&cluster],
    );
    let bucket = make_resource("aws.s3.Bucket", "cluster/logs", "logs", &[]);
    let cluster_id = cluster.id.clone();
    let bucket_id = bucket.id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(cluster)));
    plan.add(Effect::Create(resolved(node_role)));
    plan.add(Effect::Create(resolved(bucket)));

    let cluster_site = CallSite::new(
        EphemeralId::new(carina_core::resource::ResolvedResourceId::new(
            ResourceId::with_identity("_virtual", "cluster"),
        )),
        "./modules/cluster",
    );
    let mut trace = ExpansionTrace::new();
    trace.record(PersistentId::new(cluster_id), vec![cluster_site.clone()]);
    trace.record(PersistentId::new(bucket_id), vec![cluster_site]);

    let output = strip_ansi(&format_plan_for_test(
        color_mode,
        &plan,
        &PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        ),
        DetailLevel::None,
        &HashMap::new(),
        Some(&SchemaRegistry::new()),
        &HashMap::new(),
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
    let color_mode = crate::output_style::ColorMode::Plain;
    let from_id = ResourceId::with_identity("aws.s3.Bucket", "cluster/old_logs");
    let to_id = ResourceId::with_identity("aws.s3.Bucket", "cluster/logs");
    let mut updated = Resource::new("aws.s3.Bucket", "cluster/logs");
    updated.id = to_id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id),
        to: carina_core::resource::ResolvedResourceId::new(to_id.clone()),
        origin: None,
    });
    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("logs-id"),
        from: Box::new(State::existing(to_id.clone(), HashMap::new()).with_identifier("logs-id")),
        to: resolved(updated),
        changed_attributes: Vec::new(),
    });

    let moved_origins = HashMap::new();
    let historical_display = HistoricalDisplayAttributes::for_plan(&plan);
    let provider = crate::fixture_plan::provider_router_for_plan(
        &crate::test_support::TestCommandRuntime::new(),
        &plan,
    )
    .expect("fixture mock inputs must be valid");
    let mut ctx = TreeRenderContext {
        out: format!(
            "{}\n",
            strip_ansi(&format_composition_header(
                color_mode,
                "cluster",
                Some("./modules/cluster")
            ))
        ),
        printed: HashSet::new(),
        display: PlanDisplay::unprepared(
            &plan,
            &historical_display,
            carina_core::detail_rows::DetailComparisons::RecordedEffects,
        ),
        dependents: HashMap::new(),
        replacement_create_info: HashMap::new(),
        replacement_delete_indices: HashSet::new(),
        detail: DetailLevel::None,
        schemas: None,
        moved_origins: &moved_origins,
        prev_explicit: None,
        update_or_replace_targets: [to_id].into_iter().collect(),
        provider: &provider,
    };

    let rendered_attrs = ctx
        .render_children(
            color_mode,
            &[ChildRenderItem::Normal(0)],
            ChildRenderOptions {
                parent_indent: 0,
                parent_is_last: true,
                parent_prefix: "",
                parent_binding: None,
                parent_displayed_attrs: false,
                child_prefix_override: Some(module_child_prefix()),
            },
        )
        .expect("display fixture must have an initialized provider route");

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
    let color_mode = crate::output_style::ColorMode::Plain;
    let mut parent = make_resource("aws.s3.Bucket", "parent", "parent", &[]);
    parent.set_attr(
        "bucket",
        Value::Concrete(ConcreteValue::String("parent".into())),
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
        origin: None,
    });
    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("child-id"),
        from: Box::new(State::existing(to_id.clone(), HashMap::new()).with_identifier("child-id")),
        to: resolved(updated),
        changed_attributes: Vec::new(),
    });

    let moved_origins = HashMap::new();
    let historical_display = HistoricalDisplayAttributes::for_plan(&plan);
    let provider = crate::fixture_plan::provider_router_for_plan(
        &crate::test_support::TestCommandRuntime::new(),
        &plan,
    )
    .expect("fixture mock inputs must be valid");
    let mut ctx = TreeRenderContext {
        out: String::new(),
        printed: HashSet::new(),
        display: PlanDisplay::unprepared(
            &plan,
            &historical_display,
            carina_core::detail_rows::DetailComparisons::RecordedEffects,
        ),
        dependents: HashMap::from([(0, vec![1])]),
        replacement_create_info: HashMap::new(),
        replacement_delete_indices: HashSet::new(),
        detail: DetailLevel::Full,
        schemas: None,
        moved_origins: &moved_origins,
        prev_explicit: None,
        update_or_replace_targets: [to_id].into_iter().collect(),
        provider: &provider,
    };

    let rendered_attrs = ctx
        .format_render_item(color_mode, &ChildRenderItem::Normal(0), 0, true, "", None)
        .expect("display fixture must have an initialized provider route");
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

#[test]
fn simultaneous_ansi_and_plain_rendering_keep_local_policy() {
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|threads| {
        let a_start = start.clone();
        let ansi = threads.spawn(move || {
            let rendered = render_value(&concrete(ConcreteValue::String("one\ntwo".into())));
            a_start.wait();
            let output = render_value_ansi(
                crate::output_style::ColorMode::Ansi,
                &rendered,
                AnsiValueStyle::Semantic,
                Some("  │ "),
            );
            assert!(output.contains('\x1b'));
            output
        });
        let plain = threads.spawn(move || {
            let rendered = render_value(&concrete(ConcreteValue::String("one\ntwo".into())));
            start.wait();
            let output = render_value_ansi(
                crate::output_style::ColorMode::Plain,
                &rendered,
                AnsiValueStyle::Semantic,
                Some("  │ "),
            );
            assert!(!output.contains('\x1b'));
            output
        });
        assert_eq!(strip_ansi(&ansi.join().unwrap()), plain.join().unwrap());
    });
}
