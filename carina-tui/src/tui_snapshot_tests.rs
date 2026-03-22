//! TUI snapshot tests using ratatui TestBackend.
//!
//! Renders the TUI to an in-memory buffer and snapshots the output,
//! ensuring the plan viewer displays correctly.

use std::collections::{HashMap, HashSet};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;

use crate::app::App;
use crate::ui::draw;

/// Convert a ratatui Buffer to a string, trimming trailing whitespace per line.
fn buffer_to_string(buffer: &Buffer) -> String {
    let mut output = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            output.push(
                buffer
                    .cell((x, y))
                    .unwrap()
                    .symbol()
                    .chars()
                    .next()
                    .unwrap_or(' '),
            );
        }
        output.push('\n');
    }
    output
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the TUI into a string, optionally selecting a specific node.
fn render_tui(plan: &Plan, width: u16, height: u16, selection: usize) -> String {
    render_tui_with_schemas(plan, &HashMap::new(), width, height, selection)
}

/// Render the TUI into a string with schemas, optionally selecting a specific node.
fn render_tui_with_schemas(
    plan: &Plan,
    schemas: &HashMap<String, ResourceSchema>,
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
        Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
    ));
    plan.add(Effect::Create(
        Resource::new("ec2.route_table", "my-rt")
            .with_attribute("_binding", Value::String("rt".to_string()))
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            ),
    ));
    plan.add(Effect::Create(
        Resource::new("ec2.subnet", "my-subnet")
            .with_attribute("_binding", Value::String("subnet".to_string()))
            .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string()))
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            ),
    ));
    plan
}

/// Build a plan with mixed operations: VPC Update + SG Create + Subnet Delete.
fn build_mixed_operations_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        id: ResourceId::new("ec2.vpc", "my-vpc"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.vpc", "my-vpc"),
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
        to: Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()))
            .with_attribute("enable_dns_support", Value::Bool(true)),
        changed_attributes: vec!["enable_dns_support".to_string()],
    });
    plan.add(Effect::Create(
        Resource::new("ec2.security_group", "my-sg")
            .with_attribute("_binding", Value::String("sg".to_string()))
            .with_attribute(
                "group_description",
                Value::String("Web security group".to_string()),
            )
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            ),
    ));
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.subnet", "old-subnet"),
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

    let old_tags: HashMap<String, Value> = [
        ("Name".to_string(), Value::String("my-vpc".to_string())),
        (
            "Environment".to_string(),
            Value::String("staging".to_string()),
        ),
        ("OldTag".to_string(), Value::String("to-remove".to_string())),
    ]
    .into_iter()
    .collect();

    let new_tags: HashMap<String, Value> = [
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
        id: ResourceId::new("ec2.vpc", "my-vpc"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.vpc", "my-vpc"),
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
        to: Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
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
        Resource::new("ec2.vpc", "my-vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
    ));

    use carina_core::schema::{AttributeSchema, AttributeType};

    let schema = ResourceSchema::new("ec2.vpc")
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

    let schemas: HashMap<String, ResourceSchema> =
        [("ec2.vpc".to_string(), schema)].into_iter().collect();

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
    let mut app = App::new(plan, &HashMap::new());

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
