//! TUI snapshot tests using ratatui TestBackend.
//!
//! Renders the TUI to an in-memory buffer and snapshots the output,
//! ensuring the plan viewer displays correctly.

use std::collections::HashSet;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use carina_core::effect::{DeferredReplaceDelete, Effect, NonEmptyDeletes};
use carina_core::parser::{DeferredForExpression, ForBinding};
use carina_core::plan::Plan;
use carina_core::resource::{
    ConcreteValue, DeferredValue, Directives, Resource, ResourceId, State, UnknownReason, Value,
};
use carina_core::schema::SchemaRegistry;

use crate::app::App;
use crate::test_utils::buffer_to_string;
use crate::ui::draw;

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
    let mut app = App::new(plan, schemas);

    for _ in 0..selection {
        app.move_down();
    }

    terminal.draw(|f| draw(f, &mut app)).unwrap();
    buffer_to_string(terminal.backend().buffer())
}

/// Build a plan with VPC + route table + subnet (all Create effects with dependencies).
fn build_all_create_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            ),
    ));
    plan.add(Effect::Create(
        Resource::new("ec2.RouteTable", "my-rt")
            .with_binding("rt")
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));
    plan.add(Effect::Create(
        Resource::new("ec2.Subnet", "my-subnet")
            .with_binding("subnet")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
            )
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));
    plan
}

/// Build a plan with mixed operations: VPC Update + SG Create + Subnet Delete.
fn build_mixed_operations_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        id: ResourceId::new("ec2.Vpc", "my-vpc"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Vpc", "my-vpc"),
            [
                (
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                ),
                (
                    "_binding".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )
            .with_attribute(
                "enable_dns_support",
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
        changed_attributes: vec!["enable_dns_support".to_string()],
    });
    plan.add(Effect::Create(
        Resource::new("ec2.SecurityGroup", "my-sg")
            .with_binding("sg")
            .with_attribute(
                "group_description",
                Value::Concrete(ConcreteValue::String("Web security group".to_string())),
            )
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Subnet", "old-subnet"),
        identifier: "subnet-12345678".to_string(),
        directives: Directives::default(),
        binding: Some("old_subnet".to_string()),
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
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
            Value::Concrete(ConcreteValue::String("my-vpc".to_string())),
        ),
        (
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("staging".to_string())),
        ),
        (
            "OldTag".to_string(),
            Value::Concrete(ConcreteValue::String("to-remove".to_string())),
        ),
    ]
    .into_iter()
    .collect();

    let new_tags: indexmap::IndexMap<String, Value> = [
        (
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("my-vpc".to_string())),
        ),
        (
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("production".to_string())),
        ),
        (
            "NewTag".to_string(),
            Value::Concrete(ConcreteValue::String("added".to_string())),
        ),
    ]
    .into_iter()
    .collect();

    plan.add(Effect::Update {
        id: ResourceId::new("ec2.Vpc", "my-vpc"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Vpc", "my-vpc"),
            [
                (
                    "_binding".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc".to_string())),
                ),
                (
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                ),
                (
                    "tags".to_string(),
                    Value::Concrete(ConcreteValue::Map(old_tags)),
                ),
            ]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )
            .with_attribute("tags", Value::Concrete(ConcreteValue::Map(new_tags))),
        changed_attributes: vec!["tags".to_string()],
    });
    plan
}

fn build_deferred_for_plan() -> Plan {
    let template_resource =
        Resource::new("route53.Record", "validation_records").with_binding("validation_records");
    let deferred = DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        attributes: deferred_record_attributes(),
        binding_name: "validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let mut plan = Plan::new();
    plan.add(Effect::Create(certificate_resource()));
    plan.add(Effect::DeferredCreate {
        id: ResourceId::new("__deferred_for", "validation_records"),
        upstream_binding: "cert".to_string(),
        template: Box::new(deferred),
    });
    plan
}

fn build_anonymous_deferred_for_plan() -> Plan {
    let template_resource = Resource::new("route53.Record", "validation_records");
    let deferred = DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        attributes: deferred_record_attributes(),
        binding_name: "_anon_validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let mut plan = Plan::new();
    plan.add(Effect::Create(certificate_resource()));
    plan.add(Effect::DeferredCreate {
        id: ResourceId::new("__deferred_for", "_anon_validation_records"),
        upstream_binding: "cert".to_string(),
        template: Box::new(deferred),
    });
    plan
}

fn build_deferred_replace_plan() -> Plan {
    let template_resource =
        Resource::new("route53.Record", "validation_records").with_binding("validation_records");
    let deferred = DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        attributes: deferred_record_attributes(),
        binding_name: "validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let mut plan = Plan::new();
    plan.add(Effect::Create(certificate_resource()));
    plan.add(Effect::DeferredReplace {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: ResourceId::new("route53.Record", "old-record-0"),
            identifier: "record-0".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::from(["cert".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        id: ResourceId::new("__deferred_for", "validation_records"),
        upstream_binding: "cert".to_string(),
        template: Box::new(deferred),
    });
    plan
}

fn certificate_resource() -> Resource {
    Resource::new("acm.Certificate", "cert")
        .with_binding("cert")
        .with_attribute(
            "domain_name",
            Value::Concrete(ConcreteValue::String("registry.example.com".to_string())),
        )
        .with_attribute(
            "validation_method",
            Value::Concrete(ConcreteValue::String("DNS".to_string())),
        )
}

fn deferred_record_attributes() -> Vec<(String, Value)> {
    vec![
        (
            "hosted_zone_id".to_string(),
            Value::Concrete(ConcreteValue::String("Z123".to_string())),
        ),
        ("ttl".to_string(), Value::Concrete(ConcreteValue::Int(60))),
        (
            "type".to_string(),
            Value::Concrete(ConcreteValue::String("CNAME".to_string())),
        ),
        (
            "name".to_string(),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
        ),
        (
            "resource_records".to_string(),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
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
fn snapshot_mixed_operations() {
    let plan = build_mixed_operations_plan();
    let output = render_tui(&plan, 120, 40, 0);
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
    plan.add(Effect::Create(
        Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            ),
    ));

    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::string()).required())
        .attribute(
            AttributeSchema::new("enable_dns_support", AttributeType::bool())
                .with_default(Value::Concrete(ConcreteValue::Bool(true))),
        )
        .attribute(
            AttributeSchema::new("enable_dns_hostnames", AttributeType::bool())
                .with_default(Value::Concrete(ConcreteValue::Bool(false))),
        )
        .attribute(AttributeSchema::new("vpc_id", AttributeType::string()).read_only())
        .attribute(
            AttributeSchema::new("default_security_group_id", AttributeType::string()).read_only(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert("", schema);

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
    let mut app = App::new(plan, &SchemaRegistry::new());

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
// Move effect dedup: Move suppressed when Update or replacement exists for same target
// ---------------------------------------------------------------------------

/// Build a plan with Move + Update for the same target (Move should be suppressed).
fn build_moved_with_changes_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: ResourceId::new("ec2.Vpc", "old_vpc"),
        to: ResourceId::new("ec2.Vpc", "new_vpc"),
    });
    plan.add(Effect::Update {
        id: ResourceId::new("ec2.Vpc", "new_vpc"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Vpc", "new_vpc"),
            [
                (
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                ),
                (
                    "_binding".to_string(),
                    Value::Concrete(ConcreteValue::String("new_vpc".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("ec2.Vpc", "new_vpc")
            .with_binding("new_vpc")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )
            .with_attribute(
                "tags",
                Value::Concrete(ConcreteValue::Map(
                    [(
                        "Name".to_string(),
                        Value::Concrete(ConcreteValue::String("updated".to_string())),
                    )]
                    .into_iter()
                    .collect(),
                )),
            ),
        changed_attributes: vec!["tags".to_string()],
    });
    plan.add(Effect::Create(
        Resource::new("ec2.Subnet", "my-subnet")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
            )
            .with_attribute(
                "vpc_id",
                Value::resource_ref("new_vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));
    plan
}

/// Build a plan with a pure Move (no Update/replacement, Move should be kept).
fn build_moved_pure_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: ResourceId::new("ec2.Vpc", "old_vpc"),
        to: ResourceId::new("ec2.Vpc", "new_vpc"),
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
