//! Map and list-of-maps diff rendering.

use carina_core::detail_rows::{
    ListOfMapsDiffField, ListOfMapsDiffItem, ListOfMapsDiffItemKind, ListOfMapsDiffModified,
    MapDiffEntryIR, hidden_unchanged_summary,
};
use carina_core::value::{PrettyLayout, format_value_pretty, needs_trailing_separator};
use ratatui::prelude::*;

/// Render string-list diff entries (#2943) into TUI lines. Mirrors
/// `carina-cli::display::render_string_list_diff_entries` — including
/// the trailing-summary placement.
pub(super) fn render_string_list_diff_entries(
    lines: &mut Vec<Line>,
    unchanged: &[String],
    added: &[String],
    removed: &[String],
) {
    for s in removed {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "- ",
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
            Span::styled(
                format!("\"{}\"", s),
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
        ]));
    }
    for s in added {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled("+ ", Style::default().fg(Color::Green)),
            Span::styled(format!("\"{}\"", s), Style::default().fg(Color::Green)),
        ]));
    }
    if !unchanged.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                hidden_unchanged_summary(unchanged.len(), "element"),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
}

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
                render_map_diff_entries(&mut nested_lines, entries.as_slice());
                for line in nested_lines {
                    let mut indented_spans = vec![Span::raw("    ")];
                    indented_spans.extend(line.spans);
                    lines.push(Line::from(indented_spans));
                }
            }
            MapDiffEntryIR::NestedListOfMapsDiff { key, block } => {
                let mut nested_lines = Vec::new();
                render_list_of_maps_diff(
                    &mut nested_lines,
                    key,
                    block.unchanged(),
                    block.modified(),
                    block.added(),
                    block.removed(),
                    false,
                );
                for line in nested_lines {
                    let mut indented_spans = vec![Span::raw("    ")];
                    indented_spans.extend(line.spans);
                    lines.push(Line::from(indented_spans));
                }
            }
            MapDiffEntryIR::StringListChanged {
                key,
                unchanged,
                added,
                removed,
            } => {
                // #3234.
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::raw(format!("{}:", key)),
                ]));
                let mut nested_lines = Vec::new();
                render_string_list_diff_entries(&mut nested_lines, unchanged, added, removed);
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
        let mut emitted = 0usize;
        for field in m.fields.iter() {
            if emitted > 0 {
                spans.push(Span::raw(", "));
            }
            emitted += 1;
            match field {
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
                ListOfMapsDiffField::StringListChanged {
                    key,
                    unchanged,
                    added,
                    removed,
                } => {
                    spans.push(Span::raw(format!("{}: ", key)));
                    lines.push(Line::from(std::mem::take(&mut spans)));
                    let mut nested_lines = Vec::new();
                    render_string_list_diff_entries(&mut nested_lines, unchanged, added, removed);
                    for line in nested_lines {
                        let mut indented = vec![Span::raw("      ")];
                        indented.extend(line.spans);
                        lines.push(Line::from(indented));
                    }
                }
            }
        }
        // #2881: surface unchanged-fields summary inside the brace block.
        // When `spans` is empty here, the previous field was a
        // `NestedMapChanged` whose recursion flushed the inline buffer
        // (`std::mem::take` at the arm above). Emit the summary on its
        // own indented line in that case so we don't produce a stray
        // `, # (... hidden)}` with no opening `{`.
        if m.hidden_unchanged_count > 0 {
            let summary = hidden_unchanged_summary(m.hidden_unchanged_count, "field");
            // Use the same dim style other TUI paths use for the
            // sibling top-level `# (n unchanged attributes hidden)`
            // row (`detail.rs::dim_style`) so both render identically.
            let dim = Style::default().fg(Color::DarkGray);
            if spans.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(summary, dim),
                ]));
            } else {
                if emitted > 0 {
                    spans.push(Span::raw(", "));
                }
                spans.push(Span::styled(summary, dim));
            }
        }
        if !spans.is_empty() {
            spans.push(Span::raw("}"));
            lines.push(Line::from(spans));
        } else {
            // Closing brace on its own line aligned with the opening.
            lines.push(Line::from(Span::raw("    }")));
        }
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
