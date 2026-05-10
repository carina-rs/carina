//! Map and list-of-maps diff rendering.

use carina_core::detail_rows::{
    ListOfMapsDiffField, ListOfMapsDiffItem, ListOfMapsDiffItemKind, ListOfMapsDiffModified,
    MapDiffEntryIR,
};
use carina_core::value::{PrettyLayout, format_value_pretty, needs_trailing_separator};
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
    added: &[ListOfMapsDiffItem],
    removed: &[ListOfMapsDiffItem],
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
        push_added_removed_block(lines, item, ListOfMapsDiffItemKind::Added);
    }
    for item in removed {
        push_added_removed_block(lines, item, ListOfMapsDiffItemKind::Removed);
    }
    lines.push(Line::from(Span::raw("  ]")));
}

/// Render a wholly added or removed list-of-maps element as a multi-line
/// `+ {` / `- {` block (#2877). Mirrors the CLI display in
/// `carina-cli/src/display/mod.rs::render_added_removed_block`. Each
/// field's value goes through `format_value_pretty` so nested long lists
/// or maps wrap to multiple indented lines instead of dumping inline.
fn push_added_removed_block(
    lines: &mut Vec<Line>,
    item: &ListOfMapsDiffItem,
    kind: ListOfMapsDiffItemKind,
) {
    let (marker, color, modifier) = match kind {
        ListOfMapsDiffItemKind::Added => ("+", Color::Green, Modifier::empty()),
        ListOfMapsDiffItemKind::Removed => ("-", Color::Red, Modifier::CROSSED_OUT),
    };
    let style = Style::default().fg(color).add_modifier(modifier);

    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(format!("{} {{", marker), style),
    ]));

    // Constant indent — nesting (e.g. `NestedListOfMapsDiff`) is handled by
    // the outer wrapper that prepends `"    "` to every line, shifting first-
    // line key and continuation indent together. So we don't need to thread a
    // dynamic prefix through here the way the CLI side does.
    let field_indent_cols = 6;
    let field_indent = " ".repeat(field_indent_cols);
    let mut prev_needs_separator = false;
    for (key, value) in &item.fields {
        // Mirror `format_map_vertical`: insert a blank line before the
        // next sibling key when the previous value was a multi-element
        // list-of-maps so the boundary stays visible (#2555).
        if prev_needs_separator {
            lines.push(Line::from(Span::raw("")));
        }
        prev_needs_separator = needs_trailing_separator(value);
        let layout = PrettyLayout {
            parent_indent_cols: field_indent_cols,
            key,
        };
        let pretty = format_value_pretty(value, layout);
        for (i, vline) in pretty.split('\n').enumerate() {
            let line = if i == 0 {
                format!("{}{}: {}", field_indent, key, vline)
            } else {
                vline.to_string()
            };
            lines.push(Line::from(Span::styled(line, style)));
        }
    }

    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled("}".to_string(), style),
    ]));
}
