//! Tree pane drawing and tree-connector helpers.

use std::collections::HashSet;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::app::{App, FocusedPanel};

use super::style::{build_plan_title, effect_style};

/// Draw the tree view (compact, no inline attributes)
pub(super) fn draw_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    let visible = app.visible_nodes();
    let visible_set: HashSet<usize> = visible.iter().copied().collect();

    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(row_idx, &node_idx)| {
            let node = &app.nodes[node_idx];
            let connector = build_tree_connector_filtered(node_idx, app, &visible_set);
            let is_ancestor_only = app.is_ancestor_only(node_idx);
            let effect_color = if is_ancestor_only {
                Style::default().fg(Color::DarkGray)
            } else {
                effect_style(node.kind)
            };
            let type_style = if is_ancestor_only {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            };
            let connector_style = if is_ancestor_only {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(connector, connector_style),
                Span::styled(format!("{} ", node.symbol), effect_color),
                Span::styled(node.resource_type.clone(), type_style),
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
    let tree_border_color = if app.focused_panel == FocusedPanel::Tree {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title_line)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(tree_border_color)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    // Update the tree area height (inner area = total height minus 2 for borders)
    app.tree_area_height = area.height.saturating_sub(2) as usize;

    let mut state = app.list_state.clone();
    // Force our manually-tracked scroll offset to prevent ratatui's auto-scrolling
    *state.offset_mut() = app.tree_scroll_offset;
    frame.render_stateful_widget(list, area, &mut state);
}

/// Build the tree connector prefix for a node, considering filtered visibility.
///
/// When nodes are filtered, the connector uses the visible children of each
/// parent rather than all children, so `└──` / `├──` are correct for the
/// filtered tree.
fn build_tree_connector_filtered(idx: usize, app: &App, visible_set: &HashSet<usize>) -> String {
    let node = &app.nodes[idx];
    if node.parent.is_none() {
        return String::new();
    }

    let mut parts: Vec<&str> = Vec::new();

    // This node's own connector
    if let Some(parent_idx) = node.parent {
        let visible_siblings: Vec<usize> = app.nodes[parent_idx]
            .children
            .iter()
            .copied()
            .filter(|c| visible_set.contains(c))
            .collect();
        let is_last = visible_siblings.last() == Some(&idx);
        if is_last {
            parts.push("└── ");
        } else {
            parts.push("├── ");
        }
    }

    // Walk up ancestors
    let mut ancestor = node.parent;
    while let Some(a_idx) = ancestor {
        let a_node = &app.nodes[a_idx];
        if a_node.parent.is_none() {
            break;
        }
        if let Some(grandparent_idx) = a_node.parent {
            let visible_siblings: Vec<usize> = app.nodes[grandparent_idx]
                .children
                .iter()
                .copied()
                .filter(|c| visible_set.contains(c))
                .collect();
            let is_last = visible_siblings.last() == Some(&a_idx);
            if is_last {
                parts.push("    ");
            } else {
                parts.push("│   ");
            }
        }
        ancestor = a_node.parent;
    }

    parts.reverse();
    format!("    {}", parts.join(""))
}

/// Build the tree connector prefix for a node (unfiltered, for tests).
#[cfg(test)]
fn build_tree_connector(idx: usize, app: &App) -> String {
    let all_nodes: HashSet<usize> = (0..app.nodes.len()).collect();
    build_tree_connector_filtered(idx, app, &all_nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::{Resource, Value};
    use carina_core::schema::SchemaRegistry;

    #[test]
    fn tree_connector_root_has_no_prefix() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.Bucket", "my-bucket")));
        let app = App::new(&plan, &SchemaRegistry::new());
        assert_eq!(build_tree_connector(0, &app), "");
    }

    #[test]
    fn tree_connector_single_child() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.Vpc", "my-vpc")
                .with_binding("vpc")
                .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.Subnet", "my-subnet")
                .with_binding("subnet")
                .with_attribute(
                    "vpc_id",
                    Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
                ),
        ));
        let app = App::new(&plan, &SchemaRegistry::new());

        // Subnet is the only (last) child of VPC
        let connector = build_tree_connector(1, &app);
        assert_eq!(connector, "    └── ");
    }

    #[test]
    fn tree_connector_multiple_children() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.Vpc", "my-vpc").with_binding("vpc"),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.Subnet", "subnet-a")
                .with_binding("subnet_a")
                .with_attribute(
                    "vpc_id",
                    Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
                ),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.Subnet", "subnet-b")
                .with_binding("subnet_b")
                .with_attribute(
                    "vpc_id",
                    Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
                ),
        ));
        let app = App::new(&plan, &SchemaRegistry::new());

        // First child gets ├─, last child gets └─
        let children = &app.nodes[0].children;
        assert_eq!(children.len(), 2);
        let first_child = children[0];
        let last_child = children[1];
        assert_eq!(build_tree_connector(first_child, &app), "    ├── ");
        assert_eq!(build_tree_connector(last_child, &app), "    └── ");
    }
}
