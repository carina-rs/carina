//! UI rendering for the TUI plan viewer

use std::collections::HashSet;

use carina_core::detail_rows::{
    DetailRow, ListOfMapsDiffField, ListOfMapsDiffModified, MapDiffEntryIR,
};
use carina_core::plan::PlanSummary;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::app::{App, EffectKind, FocusedPanel};

/// Draw the main layout: tree (70%), detail panel (30%), help/search bar (1 line)
pub fn draw(frame: &mut Frame, app: &mut App) {
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

    if app.search_active {
        draw_search_bar(frame, app, chunks[2]);
    } else {
        draw_help(frame, app, chunks[2]);
    }
}

/// Draw the tree view (compact, no inline attributes)
fn draw_tree(frame: &mut Frame, app: &mut App, area: Rect) {
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

/// Draw the detail panel showing attributes of the selected node
fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
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
            render_detail_row_to_lines(&mut lines, row, node.kind, is_selected);
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

/// Split a string by `, ` at the top level, respecting nested brackets, braces, and quotes.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut in_quote = false;
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_quote = !in_quote,
            b'[' | b'{' if !in_quote => depth += 1,
            b']' | b'}' if !in_quote => depth -= 1,
            b',' if !in_quote && depth == 0 => {
                if i + 1 < bytes.len() && bytes[i + 1] == b' ' {
                    parts.push(&s[start..i]);
                    start = i + 2;
                    i += 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Infer a color for a rendered attribute value based on its string form.
///
/// - Quoted strings (`"..."`) → Green
/// - Booleans (`true` / `false`) → Yellow
/// - Numbers (integer or float) → default (White)
/// - DSL identifiers (dot-separated, e.g. `awscc.Region.ap_northeast_1`) → Magenta
/// - Everything else → None (caller decides)
fn value_color(rendered: &str) -> Option<Color> {
    if rendered.starts_with('"') && rendered.ends_with('"') {
        return Some(Color::Green);
    }
    if rendered == "true" || rendered == "false" {
        return Some(Color::Yellow);
    }
    // Integer or float
    if !rendered.is_empty()
        && rendered
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        // Must start with a digit or '-'
        let first = rendered.chars().next().unwrap();
        if first.is_ascii_digit() || first == '-' {
            return Some(Color::White);
        }
    }
    // DSL identifier: contains dots, no quotes, no spaces (e.g. binding.attr or awscc.Region.x)
    // ResourceRef is handled separately (cyan), so this catches remaining dot-notation identifiers
    if rendered.contains('.') && !rendered.contains(' ') && !rendered.starts_with('{') {
        return Some(Color::Magenta);
    }
    None
}

/// Build styled spans for a rendered value, coloring sub-elements individually for
/// lists and maps.
fn value_spans<'a>(rendered: &str, ref_binding: bool) -> Vec<Span<'a>> {
    let base_style = if ref_binding {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    // List: color each element individually
    if rendered.starts_with('[') && rendered.ends_with(']') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), base_style)];
        }
        let elements = split_top_level(inner);
        let mut spans = vec![Span::raw("[")];
        for (i, elem) in elements.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(", "));
            }
            spans.extend(value_spans(elem.trim(), ref_binding));
        }
        spans.push(Span::raw("]"));
        return spans;
    }

    // Map: color each value individually
    if rendered.starts_with('{') && rendered.ends_with('}') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), base_style)];
        }
        let entries = split_top_level(inner);
        let mut spans = vec![Span::raw("{")];
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(", "));
            }
            if let Some(colon_pos) = entry.find(": ") {
                let key = &entry[..colon_pos];
                let val = &entry[colon_pos + 2..];
                spans.push(Span::raw(format!("{}: ", key)));
                spans.extend(value_spans(val, false));
            } else {
                spans.push(Span::raw(entry.to_string()));
            }
        }
        spans.push(Span::raw("}"));
        return spans;
    }

    // Atomic value
    let style = if ref_binding {
        Style::default().fg(Color::Cyan)
    } else if let Some(color) = value_color(rendered) {
        Style::default().fg(color)
    } else {
        Style::default()
    };
    vec![Span::styled(rendered.to_string(), style)]
}

/// Build styled spans for a rendered value with dimmed modifier (for default values).
fn value_spans_dimmed<'a>(rendered: &str) -> Vec<Span<'a>> {
    let dim_style = Style::default().fg(Color::DarkGray);

    // List: color each element individually
    if rendered.starts_with('[') && rendered.ends_with(']') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), dim_style)];
        }
        let elements = split_top_level(inner);
        let mut spans = vec![Span::styled("[".to_string(), dim_style)];
        for (i, elem) in elements.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(", ".to_string(), dim_style));
            }
            spans.extend(value_spans_dimmed(elem.trim()));
        }
        spans.push(Span::styled("]".to_string(), dim_style));
        return spans;
    }

    // Map: color each value individually
    if rendered.starts_with('{') && rendered.ends_with('}') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), dim_style)];
        }
        let entries = split_top_level(inner);
        let mut spans = vec![Span::styled("{".to_string(), dim_style)];
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(", ".to_string(), dim_style));
            }
            if let Some(colon_pos) = entry.find(": ") {
                let key = &entry[..colon_pos];
                let val = &entry[colon_pos + 2..];
                spans.push(Span::styled(format!("{}: ", key), dim_style));
                spans.extend(value_spans_dimmed(val));
            } else {
                spans.push(Span::styled(entry.to_string(), dim_style));
            }
        }
        spans.push(Span::styled("}".to_string(), dim_style));
        return spans;
    }

    // Atomic value
    let style = if let Some(color) = value_color(rendered) {
        Style::default().fg(color).add_modifier(Modifier::DIM)
    } else {
        dim_style
    };
    vec![Span::styled(rendered.to_string(), style)]
}

/// Render a single `DetailRow` into TUI `Line`s.
fn render_detail_row_to_lines(
    lines: &mut Vec<Line>,
    row: &DetailRow,
    kind: EffectKind,
    is_selected: bool,
) {
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
        DetailRow::MapExpanded { key, entries } => {
            let mut header_line = Line::from(vec![Span::raw(format!("  {}:", key))]);
            if is_selected {
                header_line = header_line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(header_line);
            for entry in entries {
                let mut spans = vec![Span::raw(format!("    {}: ", entry.key))];
                spans.extend(value_spans(&entry.value, false));
                if let Some(ann) = &entry.annotation {
                    spans.push(Span::styled(
                        format!("  {}", ann),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
        DetailRow::ListOfMaps { key, items } => {
            let value_style = if kind == EffectKind::Create {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            let mut first_line = Line::from(vec![
                Span::raw(format!("  {}: ", key)),
                Span::styled("[", value_style),
            ]);
            if is_selected {
                first_line = first_line.style(Style::default().bg(Color::DarkGray));
            }
            lines.push(first_line);
            for item in items {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{{{}}}", item.fields), value_style),
                ]));
            }
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("]", value_style),
            ]));
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

/// Render map diff entries into TUI lines.
fn render_map_diff_entries(lines: &mut Vec<Line>, entries: &[MapDiffEntryIR]) {
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
        }
    }
}

/// Render list-of-maps diff into TUI lines.
fn render_list_of_maps_diff(
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

/// Draw the search input bar at the bottom of the screen
fn draw_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    let match_info = if app.search_query.is_empty() {
        String::new()
    } else if app.search_matches.is_empty() {
        " [no matches]".to_string()
    } else {
        format!(" [{}/{}]", app.current_match + 1, app.search_matches.len())
    };
    let help_text = "  Enter confirm  Esc cancel  Tab complete";
    let search = Paragraph::new(Line::from(vec![
        Span::styled(
            "/",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(&app.search_query),
        Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        Span::styled(match_info, Style::default().fg(Color::DarkGray)),
        Span::styled(help_text, Style::default().fg(Color::DarkGray)),
    ]));
    frame.render_widget(search, area);
}

/// Draw the help bar (search mode shows its own help via draw_search_bar)
fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let spans = if !app.search_matches.is_empty() {
        // Filter active: show filter-related help
        vec![
            Span::styled(
                " n/N",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" next/prev match  "),
            Span::styled(
                "/",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" search  "),
            Span::styled(
                "q/Esc",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" clear filter"),
        ]
    } else {
        // Normal mode: no filter active
        let mut spans = vec![
            Span::styled(
                " /",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" search  "),
            Span::styled(
                "\u{2191}\u{2193}/jk",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" navigate  "),
            Span::styled(
                "Tab",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" switch panel  "),
        ];
        // Show Enter hint based on focused panel
        if app.focused_panel == FocusedPanel::Tree {
            spans.push(Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" detail  "));
        } else if app.selected_detail_ref_binding().is_some() {
            spans.push(Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" follow ref  "));
        }
        if !app.nav_stack.is_empty() {
            spans.push(Span::styled(
                "Backspace",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" go back  "));
        }
        spans.push(Span::styled(
            "q/Esc",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" quit"));
        spans
    };

    let help = Paragraph::new(Line::from(spans));
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
        let app = App::new(&plan, &std::collections::HashMap::new());
        assert_eq!(build_tree_connector(0, &app), "");
    }

    #[test]
    fn tree_connector_single_child() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_binding("vpc")
                .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "my-subnet")
                .with_binding("subnet")
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                        field_path: vec![],
                    },
                ),
        ));
        let app = App::new(&plan, &std::collections::HashMap::new());

        // Subnet is the only (last) child of VPC
        let connector = build_tree_connector(1, &app);
        assert_eq!(connector, "    └── ");
    }

    #[test]
    fn tree_connector_multiple_children() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc").with_binding("vpc"),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "subnet-a")
                .with_binding("subnet_a")
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                        field_path: vec![],
                    },
                ),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "subnet-b")
                .with_binding("subnet_b")
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                        field_path: vec![],
                    },
                ),
        ));
        let app = App::new(&plan, &std::collections::HashMap::new());

        // First child gets ├─, last child gets └─
        let children = &app.nodes[0].children;
        assert_eq!(children.len(), 2);
        let first_child = children[0];
        let last_child = children[1];
        assert_eq!(build_tree_connector(first_child, &app), "    ├── ");
        assert_eq!(build_tree_connector(last_child, &app), "    └── ");
    }

    #[test]
    fn value_color_quoted_string_is_green() {
        assert_eq!(value_color("\"hello\""), Some(Color::Green));
        assert_eq!(value_color("\"10.0.0.0/16\""), Some(Color::Green));
        assert_eq!(value_color("\"\""), Some(Color::Green));
    }

    #[test]
    fn value_color_boolean_is_yellow() {
        assert_eq!(value_color("true"), Some(Color::Yellow));
        assert_eq!(value_color("false"), Some(Color::Yellow));
    }

    #[test]
    fn value_color_number_is_white() {
        assert_eq!(value_color("42"), Some(Color::White));
        assert_eq!(value_color("3.14"), Some(Color::White));
        assert_eq!(value_color("-1"), Some(Color::White));
        assert_eq!(value_color("0"), Some(Color::White));
    }

    #[test]
    fn value_color_dsl_identifier_is_magenta() {
        // DSL identifiers with dots (not quoted, not ResourceRef which is handled separately)
        assert_eq!(
            value_color("awscc.Region.ap_northeast_1"),
            Some(Color::Magenta)
        );
        assert_eq!(
            value_color("aws.s3.VersioningStatus.Enabled"),
            Some(Color::Magenta)
        );
    }

    #[test]
    fn value_color_other_values_return_none() {
        assert_eq!(value_color("[1, 2, 3]"), None);
        assert_eq!(value_color("{key: val}"), None);
        assert_eq!(value_color(""), None);
    }

    #[test]
    fn split_top_level_simple() {
        assert_eq!(split_top_level(r#""a", "b""#), vec![r#""a""#, r#""b""#]);
    }

    #[test]
    fn split_top_level_nested() {
        assert_eq!(split_top_level("[1, 2], [3]"), vec!["[1, 2]", "[3]"]);
    }

    #[test]
    fn value_spans_list_creates_multiple_spans() {
        let spans = value_spans(r#"["hello", 42]"#, false);
        // Should have: "[", "hello" (green), ", ", "42" (white), "]"
        assert!(spans.len() > 1, "List should produce multiple spans");
        // Check that individual elements got colored
        let has_green = spans.iter().any(|s| s.style.fg == Some(Color::Green));
        assert!(has_green, "Quoted string element should be green");
        let has_white = spans.iter().any(|s| s.style.fg == Some(Color::White));
        assert!(has_white, "Number element should be white");
    }

    #[test]
    fn value_spans_map_colors_values() {
        let spans = value_spans(r#"{Name: "test"}"#, false);
        assert!(spans.len() > 1, "Map should produce multiple spans");
        let has_green = spans.iter().any(|s| s.style.fg == Some(Color::Green));
        assert!(has_green, "Quoted string value should be green");
    }

    #[test]
    fn value_spans_ref_binding_cyan() {
        let spans = value_spans("[binding.attr]", true);
        let has_cyan = spans.iter().any(|s| s.style.fg == Some(Color::Cyan));
        assert!(has_cyan, "Ref binding elements should be cyan");
    }

    #[test]
    fn value_spans_dimmed_list() {
        let spans = value_spans_dimmed(r#"["hello"]"#);
        assert!(spans.len() > 1, "Dimmed list should produce multiple spans");
        let has_dim_green = spans.iter().any(|s| {
            s.style.fg == Some(Color::Green) && s.style.add_modifier.contains(Modifier::DIM)
        });
        assert!(has_dim_green, "Dimmed list string should be green+dim");
    }
}
