//! TUI snapshot tests using ratatui TestBackend.
//!
//! Renders the TUI to an in-memory buffer and snapshots the output,
//! ensuring the plan viewer displays correctly.

use std::collections::HashSet;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use carina_core::effect::{DeferredReplaceDelete, DeferredReplacePayload, Effect, NonEmptyDeletes};
use carina_core::parser::{DeferredForExpression, ForBinding};
use carina_core::plan::Plan;
use carina_core::resource::{
    AbsoluteBindingAddress, ConcreteValue, DeferredValue, Directives, LoopSlot, ModuleSource,
    ResolvedResource, Resource, ResourceId, State, Value,
};
use carina_core::schema::SchemaRegistry;

use crate::app::App;
use crate::test_utils::buffer_to_string;
use crate::ui::draw;

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn create(resource: Resource) -> Effect {
    Effect::Create(resolved(resource))
}

fn absolute(binding: &str) -> AbsoluteBindingAddress {
    carina_core::resource::absolute_binding_address_for_test(binding)
}

fn root_ref(binding: &str, attribute: &str) -> Value {
    carina_core::resource::AuthoredValue::resource_ref(binding, attribute, Vec::new())
        .resolve_root_for_test()
        .into_expanded()
        .expect("TUI snapshot fixture contains no parser placeholder")
}

/// Render the TUI into a string, optionally selecting a specific node.
fn render_tui(plan: &Plan, width: u16, height: u16, selection: usize) -> String {
    render_tui_with_schemas(plan, &SchemaRegistry::new(), width, height, selection)
}

/// Render the TUI into a string with schemas, optionally selecting a specific node.
fn render_tui_with_schemas(
    plan: &Plan,
    schemas: &SchemaRegistry,
    width: u16,
    height: u16,
    selection: usize,
) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::for_plan(plan, schemas);

    for _ in 0..selection {
        app.move_down();
    }

    terminal.draw(|f| draw(f, &mut app)).unwrap();
    buffer_to_string(terminal.backend().buffer())
}

/// Build a plan with VPC + route table + subnet (all Create effects with dependencies).
fn build_all_create_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(create(
        Resource::new("ec2.Vpc", "my-vpc")
            .with_absolute_binding(absolute("vpc"))
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            ),
    ));
    plan.add(create(
        Resource::new("ec2.RouteTable", "my-rt")
            .with_absolute_binding(absolute("rt"))
            .with_attribute("vpc_id", root_ref("vpc", "vpc_id")),
    ));
    plan.add(create(
        Resource::new("ec2.Subnet", "my-subnet")
            .with_absolute_binding(absolute("subnet"))
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.1.0/24".into())),
            )
            .with_attribute("vpc_id", root_ref("vpc", "vpc_id")),
    ));
    plan
}

/// Build a plan with mixed operations: VPC Update + SG Create + Subnet Delete.
fn build_mixed_operations_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("my-vpc-id"),
        from: Box::new(
            State::existing(
                ResourceId::with_identity("ec2.Vpc", "my-vpc"),
                [(
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
                )]
                .into_iter()
                .collect(),
            )
            .with_identifier("my-vpc-id"),
        ),
        to: resolved(
            Resource::new("ec2.Vpc", "my-vpc")
                .with_absolute_binding(absolute("vpc"))
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
                )
                .with_attribute(
                    "enable_dns_support",
                    Value::Concrete(ConcreteValue::Bool(true)),
                ),
        ),
        changed_attributes: vec!["enable_dns_support".to_string()],
    });
    plan.add(create(
        Resource::new("ec2.SecurityGroup", "my-sg")
            .with_absolute_binding(absolute("sg"))
            .with_attribute(
                "group_description",
                Value::Concrete(ConcreteValue::String("Web security group".into())),
            )
            .with_attribute("vpc_id", root_ref("vpc", "vpc_id")),
    ));
    plan.add(Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Subnet",
            "old-subnet",
        )),
        identifier: crate::fixture_applied_identifier("subnet-12345678"),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute("old_subnet")),
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    plan
}

fn build_deposed_delete_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Vpc", "main",
        )),
        identifier: crate::fixture_applied_identifier("vpc-old"),
        generation: carina_core::effect::EffectGeneration::Deposed(
            carina_core::resource::DeposedKey::new_unique(),
        ),
        directives: Directives::default(),
        binding: Some(absolute("main")),
        dependencies: HashSet::new(),
        explicit_dependencies: HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    plan
}

/// Build a plan with map key-level diffs (tag changes).
fn build_map_key_diff_plan() -> Plan {
    let mut plan = Plan::new();

    let old_tags: indexmap::IndexMap<String, Value> = [
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("my-vpc".into())),
        ),
        (
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("staging".into())),
        ),
        (
            "OldTag".to_string(),
            Value::Concrete(ConcreteValue::String("to-remove".into())),
        ),
    ]
    .into_iter()
    .collect();

    let new_tags: indexmap::IndexMap<String, Value> = [
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("my-vpc".into())),
        ),
        (
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("production".into())),
        ),
        (
            "NewTag".to_string(),
            Value::Concrete(ConcreteValue::String("added".into())),
        ),
    ]
    .into_iter()
    .collect();

    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("my-vpc-id"),
        from: Box::new(
            State::existing(
                ResourceId::with_identity("ec2.Vpc", "my-vpc"),
                [
                    (
                        "cidr_block".to_string(),
                        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
                    ),
                    (
                        "tags".to_string(),
                        Value::Concrete(ConcreteValue::Map(old_tags)),
                    ),
                ]
                .into_iter()
                .collect(),
            )
            .with_identifier("my-vpc-id"),
        ),
        to: resolved(
            Resource::new("ec2.Vpc", "my-vpc")
                .with_absolute_binding(absolute("vpc"))
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
                )
                .with_attribute("tags", Value::Concrete(ConcreteValue::Map(new_tags))),
        ),
        changed_attributes: vec!["tags".to_string()],
    });
    plan
}

fn build_deferred_for_plan() -> Plan {
    let mut template_resource: Resource<ModuleSource, LoopSlot> =
        Resource::new("route53.Record", "validation_records")
            .with_absolute_binding(absolute("validation_records"));
    template_resource
        .attributes
        .extend(deferred_record_attributes());
    let deferred = DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        target: carina_core::parser::DeferredForTarget::Bound {
            binding: absolute("validation_records"),
        },
        iterable: root_ref("cert", "domain_validation_options"),
        binding: ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let mut plan = Plan::new();
    plan.add(create(certificate_resource()));
    plan.add(Effect::DeferredCreate {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(deferred),
    });
    plan
}

fn build_anonymous_deferred_for_plan() -> Plan {
    let mut template_resource: Resource<ModuleSource, LoopSlot> =
        Resource::new("route53.Record", "validation_records");
    template_resource
        .attributes
        .extend(deferred_record_attributes());
    let parsed = carina_core::parser::parse_file(
        "for opt in cert.domain_validation_options {\n  aws.route53.Record { name = opt }\n}\n",
        std::path::Path::new("main.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("parse anonymous deferred-for target");
    let parsed = carina_core::module_resolver::resolve_modules_providerless(
        carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
        std::path::Path::new("."),
    )
    .expect("resolve anonymous deferred-for target");
    let target = parsed
        .deferred_for_expressions()
        .iter()
        .next()
        .cloned()
        .expect("anonymous deferred-for expression")
        .target;
    assert!(matches!(
        &target,
        carina_core::parser::DeferredForTarget::Unbound { .. }
    ));
    let deferred = DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        target,
        iterable: root_ref("cert", "domain_validation_options"),
        binding: ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let mut plan = Plan::new();
    plan.add(create(certificate_resource()));
    plan.add(Effect::DeferredCreate {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "_anon_validation_records",
        )),
        template: Box::new(deferred),
    });
    plan
}

fn build_deferred_replace_plan() -> Plan {
    let mut template_resource: Resource<ModuleSource, LoopSlot> =
        Resource::new("route53.Record", "validation_records")
            .with_absolute_binding(absolute("validation_records"));
    template_resource
        .attributes
        .extend(deferred_record_attributes());
    let deferred = DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        target: carina_core::parser::DeferredForTarget::Bound {
            binding: absolute("validation_records"),
        },
        iterable: root_ref("cert", "domain_validation_options"),
        binding: ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let mut plan = Plan::new();
    plan.add(create(certificate_resource()));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "old-record-0",
            )),
            identifier: crate::fixture_applied_identifier("record-0"),
            directives: Directives::default(),
            binding: Some(absolute("validation_records[0]")),
            dependencies: HashSet::from([absolute("cert")]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(deferred),
    })));
    plan
}

fn certificate_resource() -> Resource {
    Resource::new("acm.Certificate", "cert")
        .with_absolute_binding(absolute("cert"))
        .with_attribute(
            "domain_name",
            Value::Concrete(ConcreteValue::String("registry.example.com".into())),
        )
        .with_attribute(
            "validation_method",
            Value::Concrete(ConcreteValue::String("DNS".into())),
        )
}

fn deferred_record_attributes() -> Vec<(String, Value<LoopSlot>)> {
    vec![
        (
            "hosted_zone_id".to_string(),
            Value::Concrete(ConcreteValue::String("Z123".into())),
        ),
        ("ttl".to_string(), Value::Concrete(ConcreteValue::Int(60))),
        (
            "type".to_string(),
            Value::Concrete(ConcreteValue::String("CNAME".into())),
        ),
        (
            "name".to_string(),
            Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
        ),
        (
            "resource_records".to_string(),
            Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Snapshot tests
// ---------------------------------------------------------------------------

#[test]
fn snapshot_all_create() {
    let plan = build_all_create_plan();
    let output = render_tui(&plan, 120, 40, 0);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_origin_effect_labels() {
    let parsed = carina_core::parser::parse_file(
        "for name in ['one'] {\n  mock.test.resource { name = name }\n}\n",
        std::path::Path::new("main.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("parse anonymous-loop fixture");
    let parsed = carina_core::module_resolver::resolve_modules_providerless(
        carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
        std::path::Path::new("."),
    )
    .expect("resolve anonymous-loop fixture")
    .into_expanded()
    .expect("anonymous-loop fixture must not retain parser placeholders");
    let mut resource = parsed
        .resources
        .into_iter()
        .next()
        .expect("loop emits one resource");
    resource.id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "mock_test_resource_96081a11",
        None,
    );
    let origin = resource
        .loop_provenance
        .display_origin()
        .cloned()
        .expect("loop resource carries an origin");

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "_for0[0]",
            None,
        )),
        to: carina_core::resource::ResolvedResourceId::new(resource.id.clone()),
        origin: Some(origin),
    });
    plan.add(create(resource));
    let labels = App::for_plan(&plan, &SchemaRegistry::new())
        .nodes
        .into_iter()
        .map(|node| node.effect_label)
        .collect::<Vec<_>>();

    insta::assert_debug_snapshot!(labels, @r###"
    [
        "mock.test.resource _for0[0] -> mock.test.resource mock_test_resource_96081a11  (for [0] at main.crn:1)",
        "mock.test.resource (name: one)  (for [0] at main.crn:1)",
    ]
    "###);
}

#[test]
fn snapshot_mixed_operations() {
    let plan = build_mixed_operations_plan();
    let output = render_tui(&plan, 120, 40, 0);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deposed_delete() {
    let plan = build_deposed_delete_plan();
    let output = render_tui(&plan, 120, 24, 0);
    assert!(output.contains("main (deposed vpc-old)"));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_map_key_diff() {
    let plan = build_map_key_diff_plan();
    let output = render_tui(&plan, 120, 40, 0);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for_create() {
    let plan = build_deferred_for_plan();
    let output = render_tui(&plan, 120, 32, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for_anonymous() {
    let plan = build_anonymous_deferred_for_plan();
    let output = render_tui(&plan, 120, 32, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_replace() {
    let plan = build_deferred_replace_plan();
    let output = render_tui(&plan, 120, 32, 1);
    assert!(output.contains("+/-"));
    assert!(
        output.contains(
            "+/- aws.route53.Record validation_records[*] (N records after cert applies)"
        )
    );
    assert!(output.contains("<- for opt in cert.domain_validation_options"));
    assert!(!output.contains("- destroying validation_records[0]"));
    assert!(!output.contains("+ replaced by deferred for-loop"));
    insta::assert_snapshot!(output);
}

// ---------------------------------------------------------------------------
// Schema-aware tests: Create effects with defaults and read-only attributes
// ---------------------------------------------------------------------------

#[test]
fn snapshot_create_with_schema() {
    let mut plan = Plan::new();
    plan.add(create(
        Resource::new("ec2.Vpc", "my-vpc")
            .with_absolute_binding(absolute("vpc"))
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            ),
    ));

    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::required("cidr_block", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("enable_dns_support", AttributeType::bool())
                .with_default(Value::Concrete(ConcreteValue::Bool(true)))
                .build(),
        )
        .attribute(
            AttributeSchema::optional("enable_dns_hostnames", AttributeType::bool())
                .with_default(Value::Concrete(ConcreteValue::Bool(false)))
                .build(),
        )
        .attribute(AttributeSchema::provider_populated("vpc_id", AttributeType::string()).build())
        .attribute(
            AttributeSchema::provider_populated(
                "default_security_group_id",
                AttributeType::string(),
            )
            .build(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert("", schema.validate().expect("test schema must be valid"));

    let output = render_tui_with_schemas(&plan, &schemas, 120, 40, 0);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_update_unchanged_count() {
    let plan = build_mixed_operations_plan();
    // Select the VPC (first node, Update effect with 1 unchanged attribute)
    let output = render_tui(&plan, 120, 40, 0);
    insta::assert_snapshot!(output);
}

// ---------------------------------------------------------------------------
// Detail panel tests: select different nodes and verify detail content changes
// ---------------------------------------------------------------------------

#[test]
fn snapshot_detail_panel_second_node() {
    let plan = build_all_create_plan();
    let output = render_tui(&plan, 120, 40, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_detail_panel_third_node() {
    let plan = build_all_create_plan();
    let output = render_tui(&plan, 120, 40, 2);
    insta::assert_snapshot!(output);
}

// ---------------------------------------------------------------------------
// Filter mode snapshot: search query active, non-matching nodes hidden
// ---------------------------------------------------------------------------

/// Render the TUI with an active search query for filter mode testing.
fn render_tui_with_search(plan: &Plan, width: u16, height: u16, query: &str) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::for_plan(plan, &SchemaRegistry::new());

    // Simulate typing the search query and confirming with Enter
    app.search_active = true;
    app.search_query = query.to_string();
    app.update_search_matches();
    if !app.search_matches.is_empty() {
        app.jump_to_current_match();
    }
    // Confirm search (keeps filter active)
    app.search_active = false;

    terminal.draw(|f| draw(f, &mut app)).unwrap();
    buffer_to_string(terminal.backend().buffer())
}

#[test]
fn snapshot_filter_mode_subnet() {
    let plan = build_all_create_plan();
    // Search for "subnet" - should show vpc (dimmed ancestor) and subnet (match)
    let output = render_tui_with_search(&plan, 120, 40, "subnet");
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_filter_mode_route_table() {
    let plan = build_all_create_plan();
    // Search for "rt" - should show vpc (dimmed ancestor) and route table (match)
    let output = render_tui_with_search(&plan, 120, 40, "rt");
    insta::assert_snapshot!(output);
}

// ---------------------------------------------------------------------------
// Move effect dedup: Move suppressed when Update/Replace exists for same target
// ---------------------------------------------------------------------------

/// Build a plan with Move + Update for the same target (Move should be suppressed).
fn build_moved_with_changes_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Vpc", "old_vpc",
        )),
        to: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Vpc", "new_vpc",
        )),
        origin: None,
    });
    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("new-vpc-id"),
        from: Box::new(
            State::existing(
                ResourceId::with_identity("ec2.Vpc", "new_vpc"),
                [(
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
                )]
                .into_iter()
                .collect(),
            )
            .with_identifier("new-vpc-id"),
        ),
        to: resolved(
            Resource::new("ec2.Vpc", "new_vpc")
                .with_absolute_binding(absolute("new_vpc"))
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
                )
                .with_attribute(
                    "tags",
                    Value::Concrete(ConcreteValue::Map(
                        [(
                            "Name".to_string(),
                            Value::Concrete(ConcreteValue::String("updated".into())),
                        )]
                        .into_iter()
                        .collect(),
                    )),
                ),
        ),
        changed_attributes: vec!["tags".to_string()],
    });
    plan.add(create(
        Resource::new("ec2.Subnet", "my-subnet")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.1.0/24".into())),
            )
            .with_attribute("vpc_id", root_ref("new_vpc", "vpc_id")),
    ));
    plan
}

/// Build a plan with a pure Move (no Update/Replace, Move should be kept).
fn build_moved_pure_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Vpc", "old_vpc",
        )),
        to: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Vpc", "new_vpc",
        )),
        origin: None,
    });
    plan
}

#[test]
fn snapshot_moved_with_changes() {
    let plan = build_moved_with_changes_plan();
    // Move should be suppressed; only Update + Create should appear
    let output = render_tui(&plan, 120, 40, 0);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_moved_pure() {
    let plan = build_moved_pure_plan();
    // Pure move should be displayed as a tree node
    let output = render_tui(&plan, 120, 40, 0);
    insta::assert_snapshot!(output);
}
