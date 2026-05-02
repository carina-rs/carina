//! TUI snapshot tests using ratatui TestBackend.
//!
//! Renders the TUI to an in-memory buffer and snapshots the output,
//! ensuring the plan viewer displays correctly.

use std::collections::HashSet;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
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
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
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
            .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string()))
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
                    Value::String("10.0.0.0/16".to_string()),
                ),
                ("_binding".to_string(), Value::String("vpc".to_string())),
            ]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()))
            .with_attribute("enable_dns_support", Value::Bool(true)),
        changed_attributes: vec!["enable_dns_support".to_string()],
    });
    plan.add(Effect::Create(
        Resource::new("ec2.SecurityGroup", "my-sg")
            .with_binding("sg")
            .with_attribute(
                "group_description",
                Value::String("Web security group".to_string()),
            )
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Subnet", "old-subnet"),
        identifier: "subnet-12345678".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: Some("old_subnet".to_string()),
        dependencies: HashSet::new(),
    });
    plan
}

/// Build a plan with map key-level diffs (tag changes).
fn build_map_key_diff_plan() -> Plan {
    let mut plan = Plan::new();

    let old_tags: indexmap::IndexMap<String, Value> = [
        ("Name".to_string(), Value::String("my-vpc".to_string())),
        (
            "Environment".to_string(),
            Value::String("staging".to_string()),
        ),
        ("OldTag".to_string(), Value::String("to-remove".to_string())),
    ]
    .into_iter()
    .collect();

    let new_tags: indexmap::IndexMap<String, Value> = [
        ("Name".to_string(), Value::String("my-vpc".to_string())),
        (
            "Environment".to_string(),
            Value::String("production".to_string()),
        ),
        ("NewTag".to_string(), Value::String("added".to_string())),
    ]
    .into_iter()
    .collect();

    plan.add(Effect::Update {
        id: ResourceId::new("ec2.Vpc", "my-vpc"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Vpc", "my-vpc"),
            [
                ("_binding".to_string(), Value::String("vpc".to_string())),
                (
                    "cidr_block".to_string(),
                    Value::String("10.0.0.0/16".to_string()),
                ),
                ("tags".to_string(), Value::Map(old_tags)),
            ]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()))
            .with_attribute("tags", Value::Map(new_tags)),
        changed_attributes: vec!["tags".to_string()],
    });
    plan
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

// ---------------------------------------------------------------------------
// Schema-aware tests: Create effects with defaults and read-only attributes
// ---------------------------------------------------------------------------

#[test]
fn snapshot_create_with_schema() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
    ));

    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).required())
        .attribute(
            AttributeSchema::new("enable_dns_support", AttributeType::Bool)
                .with_default(Value::Bool(true)),
        )
        .attribute(
            AttributeSchema::new("enable_dns_hostnames", AttributeType::Bool)
                .with_default(Value::Bool(false)),
        )
        .attribute(AttributeSchema::new("vpc_id", AttributeType::String).read_only())
        .attribute(
            AttributeSchema::new("default_security_group_id", AttributeType::String).read_only(),
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
// Move effect dedup: Move suppressed when Update/Replace exists for same target
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
                    Value::String("10.0.0.0/16".to_string()),
                ),
                ("_binding".to_string(), Value::String("new_vpc".to_string())),
            ]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("ec2.Vpc", "new_vpc")
            .with_binding("new_vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()))
            .with_attribute(
                "tags",
                Value::Map(
                    [("Name".to_string(), Value::String("updated".to_string()))]
                        .into_iter()
                        .collect(),
                ),
            ),
        changed_attributes: vec!["tags".to_string()],
    });
    plan.add(Effect::Create(
        Resource::new("ec2.Subnet", "my-subnet")
            .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string()))
            .with_attribute(
                "vpc_id",
                Value::resource_ref("new_vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));
    plan
}

/// Build a plan with a pure Move (no Update/Replace, Move should be kept).
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
