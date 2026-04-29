//! Map and list-of-maps diff rendering.

use carina_core::detail_rows::{ListOfMapsDiffField, ListOfMapsDiffModified, MapDiffEntryIR};
use ratatui::prelude::*;

/// Render map diff entries into TUI lines.
pub(super) fn render_map_diff_entries(lines: &mut Vec<Line>, entries: &[MapDiffEntryIR]) {
    for entry in entries {
        match entry {
            MapDiffEntryIR::Changed { key, old, new } => {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled("~ ", Style::default().fg(Color::Yellow)),
                    Span::raw(format!("{}: ", key)),
                    Span::styled(
                        old.clone(),
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                    Span::raw(" -> "),
                    Span::styled(new.clone(), Style::default().fg(Color::Green)),
                ]));
            }
            MapDiffEntryIR::Added { key, value } => {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled("+ ", Style::default().fg(Color::Green)),
                    Span::raw(format!("{}: ", key)),
                    Span::styled(value.clone(), Style::default().fg(Color::Green)),
                ]));
            }
            MapDiffEntryIR::Removed { key, value } => {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        "- ",
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                    Span::styled(
                        format!("{}: ", key),
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                    Span::styled(
                        value.clone(),
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                ]));
            }
            MapDiffEntryIR::NestedMapDiff { key, entries } => {
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::raw(format!("{}:", key)),
                ]));
                let mut nested_lines = Vec::new();
                render_map_diff_entries(&mut nested_lines, entries);
                for line in nested_lines {
                    let mut indented_spans = vec![Span::raw("    ")];
                    indented_spans.extend(line.spans);
                    lines.push(Line::from(indented_spans));
                }
            }
            MapDiffEntryIR::NestedListOfMapsDiff {
                key,
                modified,
                added,
                removed,
            } => {
                let mut nested_lines = Vec::new();
                render_list_of_maps_diff(
                    &mut nested_lines,
                    key,
                    &[],
                    modified,
                    added,
                    removed,
                    false,
                );
                for line in nested_lines {
                    let mut indented_spans = vec![Span::raw("    ")];
                    indented_spans.extend(line.spans);
                    lines.push(Line::from(indented_spans));
                }
            }
        }
    }
}

/// Render list-of-maps diff into TUI lines.
pub(super) fn render_list_of_maps_diff(
    lines: &mut Vec<Line>,
    key: &str,
    unchanged: &[String],
    modified: &[ListOfMapsDiffModified],
    added: &[String],
    removed: &[String],
    is_selected: bool,
) {
    let mut first_line = Line::from(Span::raw(format!("  {}: [", key)));
    if is_selected {
        first_line = first_line.style(Style::default().bg(Color::DarkGray));
    }
    lines.push(first_line);
    for item in unchanged {
        lines.push(Line::from(Span::styled(
            format!("    {}", item),
            Style::default().fg(Color::DarkGray),
        )));
    }
    for m in modified {
        let mut spans = vec![Span::raw("    {")];
        for (i, field) in m.fields.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(", "));
            }
            match field {
                ListOfMapsDiffField::Unchanged { key, value } => {
                    spans.push(Span::raw(format!("{}: {}", key, value)));
                }
                ListOfMapsDiffField::Changed { key, old, new } => {
                    spans.push(Span::raw(format!("{}: ", key)));
                    spans.push(Span::styled(
                        old.clone(),
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ));
                    spans.push(Span::raw(" -> "));
                    spans.push(Span::styled(new.clone(), Style::default().fg(Color::Green)));
                }
                ListOfMapsDiffField::NestedMapChanged { key, entries } => {
                    // Flush current spans as a line, then render nested entries
                    spans.push(Span::raw(format!("{}: ", key)));
                    lines.push(Line::from(std::mem::take(&mut spans)));
                    let mut nested_lines = Vec::new();
                    render_map_diff_entries(&mut nested_lines, entries);
                    for line in nested_lines {
                        let mut indented = vec![Span::raw("      ")];
                        indented.extend(line.spans);
                        lines.push(Line::from(indented));
                    }
                }
            }
        }
        spans.push(Span::raw("}"));
        lines.push(Line::from(spans));
    }
    for item in added {
        lines.push(Line::from(Span::styled(
            format!("    {}", item),
            Style::default().fg(Color::Green),
        )));
    }
    for item in removed {
        lines.push(Line::from(Span::styled(
            format!("    {}", item),
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::CROSSED_OUT),
        )));
    }
    lines.push(Line::from(Span::raw("  ]")));
}
