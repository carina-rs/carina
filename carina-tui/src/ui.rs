//! UI rendering for the TUI plan viewer

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::app::{App, EffectKind, VisibleRow};

/// Draw the main layout with tree view (full width) and help bar (bottom)
pub fn draw(frame: &mut Frame, app: &App) {
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    draw_tree(frame, app, main_chunks[0]);

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
    frame.render_widget(help, main_chunks[1]);
}

/// Draw the tree view with inline attributes
fn draw_tree(frame: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible_rows();

    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(row_idx, row)| match row {
            VisibleRow::Node(idx) => {
                let node = &app.nodes[*idx];
                let connector = build_tree_connector(*idx, app);
                let expand_marker = if !node.children.is_empty() {
                    if node.expanded { "[-]" } else { "[+]" }
                } else {
                    "   "
                };
                let text = format!(
                    "{}{} {} {}",
                    connector, expand_marker, node.symbol, node.effect_label
                );
                let style = if app.selected_row == row_idx {
                    effect_style(node.kind).add_modifier(Modifier::BOLD)
                } else {
                    effect_style(node.kind)
                };
                ListItem::new(Line::from(text).style(style))
            }
            VisibleRow::Attribute {
                node_idx,
                key,
                value,
            } => {
                let node = &app.nodes[*node_idx];
                let attr_prefix = build_attribute_prefix(*node_idx, app);

                let changed_set: std::collections::HashSet<&str> =
                    node.changed_attributes.iter().map(|s| s.as_str()).collect();
                let is_changed = changed_set.contains(key.as_str());

                if is_changed {
                    let from_map: std::collections::HashMap<&str, &str> = node
                        .from_attributes
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    if let Some(old_value) = from_map.get(key.as_str()) {
                        let line = Line::from(vec![
                            Span::styled(
                                format!("{}{}: ", attr_prefix, key),
                                Style::default().fg(Color::Yellow),
                            ),
                            Span::styled(
                                old_value.to_string(),
                                Style::default()
                                    .fg(Color::Red)
                                    .add_modifier(Modifier::CROSSED_OUT),
                            ),
                            Span::styled(" -> ", Style::default().fg(Color::Yellow)),
                            Span::styled(value.clone(), Style::default().fg(Color::Green)),
                        ]);
                        ListItem::new(line)
                    } else {
                        let line = Line::from(vec![
                            Span::styled(
                                format!("{}{}: ", attr_prefix, key),
                                Style::default().fg(Color::Yellow),
                            ),
                            Span::styled(value.clone(), Style::default().fg(Color::Green)),
                        ]);
                        ListItem::new(line)
                    }
                } else {
                    let line = Line::from(Span::styled(
                        format!("{}{}: {}", attr_prefix, key, value),
                        Style::default().fg(Color::DarkGray),
                    ));
                    ListItem::new(line)
                }
            }
        })
        .collect();

    let title = format!(" Plan ({}) ", app.summary);
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
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

/// Build the tree connector prefix for a node.
///
/// Mirrors the CLI's indentation scheme:
/// - Root nodes (depth 0): no connector
/// - Children: "    ├─ " or "    └─ " with continuation lines "    │  " or "       "
fn build_tree_connector(idx: usize, app: &App) -> String {
    let node = &app.nodes[idx];
    if node.parent.is_none() {
        return String::new();
    }

    // Collect prefix segments from current node up to root
    let mut parts: Vec<&str> = Vec::new();

    // This node's own connector
    if let Some(parent_idx) = node.parent {
        let siblings = &app.nodes[parent_idx].children;
        let is_last = siblings.last() == Some(&idx);
        if is_last {
            parts.push("└─ ");
        } else {
            parts.push("├─ ");
        }
    }

    // Walk up ancestors to build continuation lines
    let mut ancestor = node.parent;
    while let Some(a_idx) = ancestor {
        let a_node = &app.nodes[a_idx];
        if a_node.parent.is_none() {
            // Ancestor is a root node — no continuation line needed
            break;
        }
        // Non-root ancestor: check if it's the last child of its parent
        if let Some(grandparent_idx) = a_node.parent {
            let siblings = &app.nodes[grandparent_idx].children;
            let is_last = siblings.last() == Some(&a_idx);
            if is_last {
                parts.push("   ");
            } else {
                parts.push("│  ");
            }
        }
        ancestor = a_node.parent;
    }

    // Reverse to get top-down order
    parts.reverse();
    // Add base indentation (4 spaces, matching CLI's base_indent + attr_base alignment)
    format!("    {}", parts.join(""))
}

/// Build the indentation prefix for attribute lines below a node.
///
/// Attributes are indented further than the node line, aligned past the
/// expand marker and symbol.
fn build_attribute_prefix(idx: usize, app: &App) -> String {
    let node = &app.nodes[idx];
    if node.parent.is_none() {
        // Root node: attributes indented with spaces to align past "[-] + type name"
        return "       ".to_string();
    }

    // For child nodes, the attribute prefix extends the tree connector with
    // continuation lines, plus extra indentation for the content.
    let mut parts: Vec<&str> = Vec::new();

    // Continuation for this node's position among siblings
    if let Some(parent_idx) = node.parent {
        let siblings = &app.nodes[parent_idx].children;
        let is_last = siblings.last() == Some(&idx);
        if is_last {
            parts.push("   ");
        } else {
            parts.push("│  ");
        }
    }

    // Walk up ancestors
    let mut ancestor = node.parent;
    while let Some(a_idx) = ancestor {
        let a_node = &app.nodes[a_idx];
        if a_node.parent.is_none() {
            // Ancestor is a root node — no continuation line needed
            break;
        }
        if let Some(grandparent_idx) = a_node.parent {
            let siblings = &app.nodes[grandparent_idx].children;
            let is_last = siblings.last() == Some(&a_idx);
            if is_last {
                parts.push("   ");
            } else {
                parts.push("│  ");
            }
        }
        ancestor = a_node.parent;
    }

    parts.reverse();
    // Extra spaces to align past "[-] + " prefix
    format!("    {}   ", parts.join(""))
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
        assert_eq!(connector, "    └─ ");
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
        assert_eq!(build_tree_connector(first_child, &app), "    ├─ ");
        assert_eq!(build_tree_connector(last_child, &app), "    └─ ");
    }

    #[test]
    fn attribute_prefix_for_root_node() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string()))
                .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
        ));
        let app = App::new(&plan);
        let prefix = build_attribute_prefix(0, &app);
        assert_eq!(prefix, "       ");
    }

    #[test]
    fn attribute_prefix_for_child_node() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string())),
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
                )
                .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string())),
        ));
        let app = App::new(&plan);
        // Subnet is last child of VPC
        let prefix = build_attribute_prefix(1, &app);
        // Should be: "    " (base) + "   " (last child continuation) + "   " (content indent)
        assert_eq!(prefix, "          ");
    }
}
