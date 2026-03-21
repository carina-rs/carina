//! TUI snapshot tests using ratatui TestBackend.
//!
//! Renders the TUI to an in-memory buffer and snapshots the output,
//! ensuring the plan viewer displays correctly.

use std::collections::{HashMap, HashSet};

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};

use crate::app::App;
use crate::ui::draw;

/// Render the TUI into a string by drawing onto a TestBackend.
fn render_tui(plan: &Plan, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(plan);
    terminal.draw(|f| draw(f, &mut app)).unwrap();

    let buffer = terminal.backend().buffer().clone();
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
    // Trim trailing whitespace from each line
    output
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the TUI with a specific node selected.
fn render_tui_with_selection(plan: &Plan, width: u16, height: u16, selection: usize) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(plan);

    // Navigate to the desired selection
    for _ in 0..selection {
        app.move_down();
    }

    terminal.draw(|f| draw(f, &mut app)).unwrap();

    let buffer = terminal.backend().buffer().clone();
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
    let output = render_tui(&plan, 120, 40);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_mixed_operations() {
    let plan = build_mixed_operations_plan();
    let output = render_tui(&plan, 120, 40);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_map_key_diff() {
    let plan = build_map_key_diff_plan();
    let output = render_tui(&plan, 120, 40);
    insta::assert_snapshot!(output);
}

// ---------------------------------------------------------------------------
// Detail panel tests: select different nodes and verify detail content changes
// ---------------------------------------------------------------------------

#[test]
fn snapshot_detail_panel_first_node() {
    let plan = build_all_create_plan();
    let output = render_tui_with_selection(&plan, 120, 40, 0);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_detail_panel_second_node() {
    let plan = build_all_create_plan();
    let output = render_tui_with_selection(&plan, 120, 40, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_detail_panel_third_node() {
    let plan = build_all_create_plan();
    let output = render_tui_with_selection(&plan, 120, 40, 2);
    insta::assert_snapshot!(output);
}
