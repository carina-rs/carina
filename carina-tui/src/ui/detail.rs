//! Detail-pane drawing and per-row rendering.

use carina_core::detail_rows::DetailRow;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, FocusedPanel};

use super::diff::{render_list_of_maps_diff, render_map_diff_entries};
use super::style::effect_style;
use super::value_view::{value_spans, value_spans_dimmed};

/// Draw the detail panel showing attributes of the selected node
pub(super) fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let detail_border_color = if app.focused_panel == FocusedPanel::Detail {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let node = match app.selected_node() {
        Some(n) => n,
        None => {
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(detail_border_color));
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

    if node.detail_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no attributes)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let is_detail_focused = app.focused_panel == FocusedPanel::Detail;
        for (row_idx, row) in node.detail_rows.iter().enumerate() {
            let is_selected = is_detail_focused && row_idx == app.detail_selected;
            render_detail_row_to_lines(&mut lines, row, is_selected);
        }
    }

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(detail_border_color)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    frame.render_widget(detail, area);
}

/// Render a single `DetailRow` into TUI `Line`s.
fn render_detail_row_to_lines(lines: &mut Vec<Line>, row: &DetailRow, is_selected: bool) {
    let dim_style = Style::default().fg(Color::DarkGray);

    match row {
        DetailRow::Attribute {
            key,
            value,
            ref_binding,
            annotation,
        } => {
            let is_navigable = ref_binding.is_some();
            let mut spans = vec![Span::raw(format!("  {}: ", key))];
            spans.extend(value_spans(value, is_navigable));
            if is_navigable {
                spans.push(Span::styled(
                    " \u{2192}",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(ann) = annotation {
                spans.push(Span::styled(
                    format!("  {}", ann),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let mut line = Line::from(spans);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::PrettyAttribute { key, value } => {
            // TUI uses a fixed 2-col prefix instead of the CLI's dynamic
            // tree-indent string, so the layout is approximated.
            let layout = carina_core::value::PrettyLayout {
                parent_indent_cols: 2,
                key,
            };
            let pretty = carina_core::value::format_value_pretty(value, layout);
            let mut spans = vec![Span::raw(format!("  {}: ", key))];
            spans.push(Span::raw(pretty));
            let mut line = Line::from(spans);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::MapExpanded { key, entries } => {
            let mut header_line = Line::from(vec![Span::raw(format!("  {}:", key))]);
            if is_selected {
                header_line = header_line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(header_line);
            let mut prev_needs_separator = false;
            for entry in entries {
                // Inject a blank line after a multi-element list-of-maps
                // before the next sibling key so the list boundary stays
                // visible — the `*` marker disambiguates element starts
                // but not element ends (#2555). Mirrors the same logic
                // in `carina-cli/src/display/mod.rs::MapExpanded`.
                if prev_needs_separator {
                    lines.push(Line::from(""));
                }
                prev_needs_separator = carina_core::value::needs_trailing_separator(&entry.value);
                let layout = carina_core::value::PrettyLayout {
                    parent_indent_cols: 4,
                    key: &entry.key,
                };
                let pretty = carina_core::value::format_value_pretty(&entry.value, layout);
                let mut spans = vec![Span::raw(format!("    {}: ", entry.key))];
                spans.extend(value_spans(&pretty, false));
                if let Some(ann) = &entry.annotation {
                    spans.push(Span::styled(
                        format!("  {}", ann),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
        DetailRow::Changed { key, old, new } => {
            let mut spans = vec![
                Span::raw(format!("  {}: ", key)),
                Span::styled(
                    old.clone(),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
                Span::raw(" -> "),
            ];
            let new_spans = value_spans(new, false);
            if new_spans.iter().all(|s| s.style == Style::default()) {
                // No specific color detected, fall back to green for changed values
                spans.push(Span::styled(new.clone(), Style::default().fg(Color::Green)));
            } else {
                spans.extend(new_spans);
            }
            let mut line = Line::from(spans);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::MapDiff { key, entries } => {
            let mut first_line = Line::from(Span::raw(format!("  {}:", key)));
            if is_selected {
                first_line = first_line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(first_line);
            render_map_diff_entries(lines, entries);
        }
        DetailRow::ListOfMapsDiff {
            key,
            unchanged,
            modified,
            added,
            removed,
        } => {
            render_list_of_maps_diff(lines, key, unchanged, modified, added, removed, is_selected);
        }
        DetailRow::Removed { key, old } => {
            let mut line = Line::from(vec![
                Span::styled(
                    format!("  {}: ", key),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
                Span::styled(
                    old.clone(),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
            ]);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::Default { key, value } => {
            let mut spans = vec![Span::styled(format!("  {}: ", key), dim_style)];
            spans.extend(value_spans_dimmed(value));
            spans.push(Span::styled("  # default", dim_style));
            let mut line = Line::from(spans);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::ReadOnly { key } => {
            let mut line = Line::from(vec![
                Span::styled(format!("  {}: ", key), dim_style),
                Span::styled("(known after apply)", dim_style),
            ]);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::HiddenUnchanged { count } => {
            let noun = if *count == 1 {
                "attribute"
            } else {
                "attributes"
            };
            let mut line = Line::from(Span::styled(
                format!("  # ({} unchanged {} hidden)", count, noun),
                dim_style,
            ));
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::ReplaceChanged { key, old, new } => {
            let mut line = Line::from(vec![
                Span::raw(format!("  {}: ", key)),
                Span::styled(
                    old.clone(),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
                Span::raw(" -> "),
                Span::styled(new.clone(), Style::default().fg(Color::Yellow)),
            ]);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::ReplaceCascade { key, old, new } => {
            let mut line = Line::from(vec![
                Span::raw(format!("  {}: ", key)),
                Span::styled(
                    old.clone(),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
                Span::raw(" -> "),
                Span::styled(new.clone(), Style::default().fg(Color::Yellow)),
            ]);
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::ReplaceListOfMapsDiff {
            key,
            unchanged,
            modified,
            added,
            removed,
        } => {
            render_list_of_maps_diff(lines, key, unchanged, modified, added, removed, is_selected);
        }
        DetailRow::ReplaceMapDiff { key, entries } => {
            let mut first_line = Line::from(Span::raw(format!("  {}:", key)));
            if is_selected {
                first_line = first_line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(first_line);
            render_map_diff_entries(lines, entries);
        }
        DetailRow::TemporaryNameNote {
            can_rename,
            temporary_value,
            original_value,
            attribute,
        } => {
            let mut line = if *can_rename {
                Line::from(Span::styled(
                    format!(
                        "  # New resource created with {attribute} = \"{temporary_value}\", \
                         then renamed to \"{original_value}\" after old resource is deleted"
                    ),
                    dim_style,
                ))
            } else {
                Line::from(Span::styled(
                    format!(
                        "  # {attribute} = \"{temporary_value}\" (temporary); \
                         original \"{original_value}\" reused after old resource deleted"
                    ),
                    dim_style,
                ))
            };
            if is_selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(line);
        }
        DetailRow::CascadingUpdates { count, updates } => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  # Cascading updates ({} dependent {}):",
                    count,
                    if *count == 1 { "resource" } else { "resources" }
                ),
                dim_style,
            )));
            for update in updates {
                lines.push(Line::from(Span::styled(
                    format!("  #   ~ {} {}", update.display_type, update.name),
                    dim_style,
                )));
                for attr in &update.changed_attrs {
                    lines.push(Line::from(vec![
                        Span::styled("  #       ", dim_style),
                        Span::styled(format!("{}: ", attr.key), dim_style),
                        Span::styled(
                            attr.old.clone(),
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::CROSSED_OUT),
                        ),
                        Span::raw(" -> "),
                        Span::styled(attr.new.clone(), Style::default().fg(Color::Yellow)),
                    ]));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::detail_rows::MapExpandedEntry;
    use carina_core::resource::Value;
    use indexmap::IndexMap;

    /// A multi-element list-of-maps entry inside a `MapExpanded` row
    /// must be followed by a blank `Line` before the next sibling
    /// entry, mirroring the CLI behavior added in #2555. A `*` marker
    /// alone is not enough to delimit the *end* of the list.
    #[test]
    fn map_expanded_inserts_blank_after_multi_element_list_of_maps() {
        let stmt = |sid: &str| {
            let mut m = IndexMap::new();
            m.insert("sid".to_string(), Value::String(sid.to_string()));
            Value::Map(m)
        };
        let row = DetailRow::MapExpanded {
            key: "policy_document".to_string(),
            entries: vec![
                MapExpandedEntry {
                    key: "statement".to_string(),
                    value: Value::List(vec![stmt("A"), stmt("B")]),
                    annotation: None,
                },
                MapExpandedEntry {
                    key: "version".to_string(),
                    value: Value::String("2012-10-17".to_string()),
                    annotation: None,
                },
            ],
        };

        let mut lines: Vec<Line> = Vec::new();
        render_detail_row_to_lines(&mut lines, &row, false);

        let version_idx = lines
            .iter()
            .position(|l| line_text(l).contains("version:"))
            .expect("version line must be rendered");
        assert!(
            version_idx >= 1,
            "version must not be the first rendered line"
        );
        assert!(
            line_text(&lines[version_idx - 1]).trim().is_empty(),
            "expected blank line immediately before version: lines={lines:?}",
        );
    }

    /// A multi-element list-of-maps that is the LAST entry in a
    /// `MapExpanded` row must NOT receive a trailing blank line —
    /// avoids orphan whitespace before the resource-block separator.
    #[test]
    fn map_expanded_no_orphan_blank_after_trailing_list_of_maps() {
        let stmt = |sid: &str| {
            let mut m = IndexMap::new();
            m.insert("sid".to_string(), Value::String(sid.to_string()));
            Value::Map(m)
        };
        let row = DetailRow::MapExpanded {
            key: "policy_document".to_string(),
            entries: vec![
                MapExpandedEntry {
                    key: "version".to_string(),
                    value: Value::String("2012-10-17".to_string()),
                    annotation: None,
                },
                MapExpandedEntry {
                    key: "statement".to_string(),
                    value: Value::List(vec![stmt("A"), stmt("B")]),
                    annotation: None,
                },
            ],
        };

        let mut lines: Vec<Line> = Vec::new();
        render_detail_row_to_lines(&mut lines, &row, false);

        let last = lines.last().expect("at least one line");
        assert!(
            !line_text(last).trim().is_empty(),
            "trailing list-of-maps must not leave an orphan blank line: lines={lines:?}",
        );
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }
}
