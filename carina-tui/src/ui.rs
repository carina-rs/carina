//! UI rendering for the TUI plan viewer

use std::collections::{HashMap, HashSet};

use carina_core::plan::PlanSummary;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::app::{App, EffectKind};

/// Draw the main layout: tree (70%), detail panel (30%), help bar (1 line)
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(70),
            Constraint::Percentage(30),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_tree(frame, app, chunks[0]);
    draw_detail(frame, app, chunks[1]);
    draw_help(frame, chunks[2]);
}

/// Draw the tree view (compact, no inline attributes)
fn draw_tree(frame: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible_nodes();

    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(row_idx, &node_idx)| {
            let node = &app.nodes[node_idx];
            let connector = build_tree_connector(node_idx, app);
            let expand_marker = if !node.children.is_empty() {
                if node.expanded { "[-]" } else { "[+]" }
            } else {
                "   "
            };
            let effect_color = effect_style(node.kind);
            let mut spans = vec![
                Span::raw(connector),
                Span::styled(format!("{} {} ", expand_marker, node.symbol), effect_color),
                Span::styled(
                    node.resource_type.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(node.name_part.clone(), effect_color),
            ];
            if app.selected == row_idx {
                spans = spans
                    .into_iter()
                    .map(|s| {
                        let mut style = s.style;
                        style = style.add_modifier(Modifier::BOLD);
                        Span::styled(s.content, style)
                    })
                    .collect();
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title_line = build_plan_title(&app.plan_summary);
    let list = List::new(items)
        .block(
            Block::default()
                .title(title_line)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = app.list_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

/// Draw the detail panel showing attributes of the selected node
fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let node = match app.selected_node() {
        Some(n) => n,
        None => {
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            frame.render_widget(block, area);
            return;
        }
    };

    let effect_color = effect_style(node.kind);
    let mut lines: Vec<Line> = Vec::new();

    // Header: symbol + resource type + name
    lines.push(Line::from(vec![
        Span::styled(format!("{} ", node.symbol), effect_color),
        Span::styled(
            node.resource_type.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(node.name_part.clone(), effect_color),
    ]));
    lines.push(Line::from(""));

    if node.attributes.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no attributes)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let changed_set: HashSet<&str> =
            node.changed_attributes.iter().map(|s| s.as_str()).collect();
        let from_map: HashMap<&str, &str> = node
            .from_attributes
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        for (key, value) in &node.attributes {
            let is_changed = changed_set.contains(key.as_str());

            if is_changed {
                if let Some(old_value) = from_map.get(key.as_str()) {
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {}: ", key)),
                        Span::styled(
                            old_value.to_string(),
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::CROSSED_OUT),
                        ),
                        Span::raw(" -> "),
                        Span::styled(value.clone(), Style::default().fg(Color::Green)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {}: ", key)),
                        Span::styled(value.clone(), Style::default().fg(Color::Green)),
                    ]));
                }
            } else {
                let value_style = if node.kind == EffectKind::Create {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::raw(format!("  {}: ", key)),
                    Span::styled(value.clone(), value_style),
                ]));
            }
        }
    }

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(detail, area);
}

/// Draw the help bar
fn draw_help(frame: &mut Frame, area: Rect) {
    let help = Paragraph::new(Line::from(vec![
        Span::styled(
            " j/k",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" navigate  "),
        Span::styled(
            "Enter",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" toggle  "),
        Span::styled(
            "l",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" expand  "),
        Span::styled(
            "h",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" collapse  "),
        Span::styled(
            "e",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" expand all  "),
        Span::styled(
            "c",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" collapse all  "),
        Span::styled(
            "q",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" quit"),
    ]));
    frame.render_widget(help, area);
}

/// Build the plan title line with colored summary counts.
///
/// Matches CLI plan output colors: create=green, update=yellow, replace=magenta,
/// delete=red, read=cyan.
fn build_plan_title(summary: &PlanSummary) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" Plan (Plan: ")];

    let mut parts_added = 0;

    if summary.read > 0 {
        if parts_added > 0 {
            spans.push(Span::raw(", "));
        }
        spans.push(Span::styled(
            format!("{}", summary.read),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw(" to read"));
        parts_added += 1;
    }

    // create is always shown
    if parts_added > 0 {
        spans.push(Span::raw(", "));
    }
    spans.push(Span::styled(
        format!("{}", summary.create),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::raw(" to create"));
    parts_added += 1;

    // update is always shown
    if parts_added > 0 {
        spans.push(Span::raw(", "));
    }
    spans.push(Span::styled(
        format!("{}", summary.update),
        Style::default().fg(Color::Yellow),
    ));
    spans.push(Span::raw(" to update"));
    parts_added += 1;

    if summary.replace > 0 {
        if parts_added > 0 {
            spans.push(Span::raw(", "));
        }
        spans.push(Span::styled(
            format!("{}", summary.replace),
            Style::default().fg(Color::Magenta),
        ));
        spans.push(Span::raw(" to replace"));
        parts_added += 1;
    }

    // delete is always shown
    if parts_added > 0 {
        spans.push(Span::raw(", "));
    }
    spans.push(Span::styled(
        format!("{}", summary.delete),
        Style::default().fg(Color::Red),
    ));
    spans.push(Span::raw(" to delete"));

    spans.push(Span::raw(") "));

    Line::from(spans)
}

/// Build the tree connector prefix for a node.
///
/// Each tree level uses exactly 4-character-wide segments:
/// - Root nodes (depth 0): no connector
/// - Children: `--- ` or `--- ` with continuation `|   ` or `    `
fn build_tree_connector(idx: usize, app: &App) -> String {
    let node = &app.nodes[idx];
    if node.parent.is_none() {
        return String::new();
    }

    // Collect prefix segments from current node up to root
    let mut parts: Vec<&str> = Vec::new();

    // This node's own connector (4 chars each)
    if let Some(parent_idx) = node.parent {
        let siblings = &app.nodes[parent_idx].children;
        let is_last = siblings.last() == Some(&idx);
        if is_last {
            parts.push("└── ");
        } else {
            parts.push("├── ");
        }
    }

    // Walk up ancestors to build continuation lines (4 chars each)
    let mut ancestor = node.parent;
    while let Some(a_idx) = ancestor {
        let a_node = &app.nodes[a_idx];
        if a_node.parent.is_none() {
            break;
        }
        if let Some(grandparent_idx) = a_node.parent {
            let siblings = &app.nodes[grandparent_idx].children;
            let is_last = siblings.last() == Some(&a_idx);
            if is_last {
                parts.push("    ");
            } else {
                parts.push("│   ");
            }
        }
        ancestor = a_node.parent;
    }

    // Reverse to get top-down order
    parts.reverse();
    // Base indentation (4 spaces) before tree connectors
    format!("    {}", parts.join(""))
}

/// Return the style for a given effect kind
fn effect_style(kind: EffectKind) -> Style {
    match kind {
        EffectKind::Create => Style::default().fg(Color::Green),
        EffectKind::Update => Style::default().fg(Color::Yellow),
        EffectKind::Replace => Style::default().fg(Color::Yellow),
        EffectKind::Delete => Style::default().fg(Color::Red),
        EffectKind::Read => Style::default().fg(Color::Cyan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::{Resource, Value};

    #[test]
    fn tree_connector_root_has_no_prefix() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "my-bucket")));
        let app = App::new(&plan);
        assert_eq!(build_tree_connector(0, &app), "");
    }

    #[test]
    fn tree_connector_single_child() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string()))
                .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "my-subnet")
                .with_attribute("_binding", Value::String("subnet".to_string()))
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                ),
        ));
        let app = App::new(&plan);

        // Subnet is the only (last) child of VPC
        let connector = build_tree_connector(1, &app);
        assert_eq!(connector, "    └── ");
    }

    #[test]
    fn tree_connector_multiple_children() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string())),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "subnet-a")
                .with_attribute("_binding", Value::String("subnet_a".to_string()))
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                ),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "subnet-b")
                .with_attribute("_binding", Value::String("subnet_b".to_string()))
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                ),
        ));
        let app = App::new(&plan);

        // First child gets ├─, last child gets └─
        let children = &app.nodes[0].children;
        assert_eq!(children.len(), 2);
        let first_child = children[0];
        let last_child = children[1];
        assert_eq!(build_tree_connector(first_child, &app), "    ├── ");
        assert_eq!(build_tree_connector(last_child, &app), "    └── ");
    }
}
