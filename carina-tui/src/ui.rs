//! UI rendering for the TUI plan viewer

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::app::{App, EffectKind};

/// Draw the main layout with tree view (left) and detail panel (right)
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(frame.area());

    draw_tree(frame, app, chunks[0]);
    draw_detail(frame, app, chunks[1]);
}

/// Draw the tree view in the left panel
fn draw_tree(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| {
            let expand_marker = if node.expanded { "[-]" } else { "[+]" };
            let text = format!("{} {} {}", expand_marker, node.symbol, node.effect_label);

            let style = effect_style(node.kind);
            let line = if idx == app.selected {
                Line::from(text).style(style.bg(Color::DarkGray))
            } else {
                Line::from(text).style(style)
            };

            ListItem::new(line)
        })
        .collect();

    let title = format!(" Plan ({}) ", app.summary);
    let list = List::new(items).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(list, area);
}

/// Draw the detail panel on the right
fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(node) = app.selected_node() else {
        let empty = Paragraph::new("No resource selected").block(
            Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White)),
        );
        frame.render_widget(empty, area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    // Header: effect type and resource ID
    let header_style = effect_style(node.kind).add_modifier(Modifier::BOLD);
    lines.push(Line::from(Span::styled(
        format!("{} {}", node.symbol, node.effect_label),
        header_style,
    )));
    lines.push(Line::from(""));

    if node.attributes.is_empty() && node.kind == EffectKind::Delete {
        lines.push(Line::from(Span::styled(
            "(resource will be deleted)",
            Style::default().fg(Color::Red),
        )));
    } else {
        // Build a set of changed attribute names for highlighting
        let changed_set: std::collections::HashSet<&str> =
            node.changed_attributes.iter().map(|s| s.as_str()).collect();

        // Build a map of old values for showing diffs
        let from_map: std::collections::HashMap<&str, &str> = node
            .from_attributes
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        for (key, value) in &node.attributes {
            let is_changed = changed_set.contains(key.as_str());

            if is_changed {
                // Show old -> new for changed attributes
                if let Some(old_value) = from_map.get(key.as_str()) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} = ", key),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            old_value.to_string(),
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::CROSSED_OUT),
                        ),
                        Span::styled(" -> ", Style::default().fg(Color::Yellow)),
                        Span::styled(value.clone(), Style::default().fg(Color::Green)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} = ", key),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(value.clone(), Style::default().fg(Color::Green)),
                    ]));
                }
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} = ", key), Style::default().fg(Color::Gray)),
                    Span::raw(value),
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
