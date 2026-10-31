//! Detail-pane drawing and per-row rendering.

use carina_core::detail_rows::{DetailRow, hidden_unchanged_summary};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, FocusedPanel};
use crate::row::{Indent, Row};

use super::diff::{
    ListOfMapsDiffHeader, render_list_of_maps_diff, render_map_diff_entries,
    render_string_list_diff_entries,
};
use super::push_annotation_span;
use super::style::effect_style;
use super::value_view::{
    ValueStyle, has_semantic_color, value_spans, value_spans_dimmed, value_spans_styled,
};

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
    let mut rows = Vec::new();

    // Header: symbol + resource type + name
    rows.push(Row::new(
        Indent::new(0),
        vec![
            Span::styled(format!("{} ", node.symbol), effect_color),
            Span::styled(
                node.resource_type.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(node.name_part.clone(), effect_color),
        ],
    ));
    rows.push(Row::blank());

    if node.detail_rows.is_empty() {
        rows.push(Row::new(
            Indent::new(0),
            vec![Span::styled(
                "(no attributes)",
                Style::default().fg(Color::DarkGray),
            )],
        ));
    } else {
        let is_detail_focused = app.focused_panel == FocusedPanel::Detail;
        for (row_idx, row) in node.detail_rows.iter().enumerate() {
            let is_selected = is_detail_focused && row_idx == app.detail_selected;
            render_detail_row_to_rows(&mut rows, row, is_selected);
        }
    }

    let lines = rows.into_iter().map(Row::into_line).collect::<Vec<_>>();
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

/// Render a single `DetailRow` into typed TUI rows.
fn render_detail_row_to_rows(rows: &mut Vec<Row<'_>>, row: &DetailRow, is_selected: bool) {
    let dim_style = Style::default().fg(Color::DarkGray);

    match row {
        DetailRow::Text { text } => {
            rows.push(
                Row::new(Indent::new(2), vec![Span::raw(text.clone())]).highlighted(is_selected),
            );
        }
        DetailRow::Attribute {
            key,
            value,
            ref_binding,
            annotation,
        } => {
            let is_navigable = ref_binding.is_some();
            let mut spans = vec![Span::raw(format!("{}: ", key))];
            spans.extend(value_spans(value));
            if is_navigable {
                spans.push(Span::styled(
                    " \u{2192}",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            push_annotation_span(&mut spans, annotation.as_deref());
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::PrettyAttribute { key, value } => {
            // TUI uses a fixed 2-col indent (`Indent::new(2)`) instead of the
            // CLI's dynamic tree-indent string, so the layout is approximated.
            let layout = carina_core::value::PrettyLayout::for_attribute(2, key);
            let pretty = value.render(layout);
            let mut spans = vec![Span::raw(format!("{}: ", key))];
            spans.extend(value_spans(&pretty));
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::MapExpanded { key, entries } => {
            rows.push(
                Row::new(Indent::new(2), vec![Span::raw(format!("{}:", key))])
                    .highlighted(is_selected),
            );
            let mut prev_needs_separator = false;
            for entry in entries {
                // Inject a blank line after a multi-element list-of-maps
                // before the next sibling key so the list boundary stays
                // visible — the `*` marker disambiguates element starts
                // but not element ends (#2555). Mirrors the same logic
                // in `carina-cli/src/display/mod.rs::MapExpanded`.
                if prev_needs_separator {
                    rows.push(Row::blank());
                }
                prev_needs_separator = entry.value.needs_trailing_separator();
                let layout = carina_core::value::PrettyLayout::for_map_key(4, &entry.key);
                let pretty = entry.value.render(layout);
                let mut spans = vec![Span::raw(format!("{}: ", entry.key))];
                spans.extend(value_spans(&pretty));
                push_annotation_span(&mut spans, entry.annotation.as_deref());
                rows.push(Row::new(Indent::new(4), spans));
            }
        }
        DetailRow::Changed { key, old, new } => {
            let mut spans = vec![Span::raw(format!("{}: ", key))];
            spans.extend(value_spans_styled(old, ValueStyle::RedStrikethrough));
            spans.push(Span::raw(" -> "));
            let new_spans = value_spans(new);
            if !has_semantic_color(new) {
                // No specific color detected, fall back to green for changed values
                spans.extend(value_spans_styled(new, ValueStyle::Green));
            } else {
                spans.extend(new_spans);
            }
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::ChangedForcesReplacement { key, old, new } => {
            let mut spans = vec![Span::raw(format!("{}: ", key))];
            spans.extend(value_spans_styled(old, ValueStyle::RedStrikethrough));
            spans.push(Span::raw(" -> "));
            spans.extend(value_spans_styled(new, ValueStyle::Yellow));
            spans.push(Span::styled(
                " (forces replacement)",
                Style::default().fg(Color::Magenta),
            ));
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::MapDiff { key, entries } => {
            rows.push(
                Row::new(Indent::new(2), vec![Span::raw(format!("{}:", key))])
                    .highlighted(is_selected),
            );
            render_map_diff_entries(rows, entries.as_slice());
        }
        DetailRow::MapDiffForcesReplacement { key, entries } => {
            rows.push(
                Row::new(
                    Indent::new(2),
                    vec![
                        Span::raw(format!("{}:", key)),
                        Span::styled(" (forces replacement)", Style::default().fg(Color::Magenta)),
                    ],
                )
                .highlighted(is_selected),
            );
            render_map_diff_entries(rows, entries.as_slice());
        }
        DetailRow::StringListDiff {
            key,
            unchanged,
            added,
            removed,
        } => {
            rows.push(
                Row::new(Indent::new(2), vec![Span::raw(format!("{}:", key))])
                    .highlighted(is_selected),
            );
            render_string_list_diff_entries(rows, unchanged, added, removed);
        }
        DetailRow::StringListDiffForcesReplacement {
            key,
            unchanged,
            added,
            removed,
        } => {
            rows.push(
                Row::new(
                    Indent::new(2),
                    vec![
                        Span::raw(format!("{}:", key)),
                        Span::styled(" (forces replacement)", Style::default().fg(Color::Magenta)),
                    ],
                )
                .highlighted(is_selected),
            );
            render_string_list_diff_entries(rows, unchanged, added, removed);
        }
        DetailRow::ListOfMapsDiff { key, block } => {
            render_list_of_maps_diff(
                rows,
                key,
                block,
                ListOfMapsDiffHeader::Standard,
                is_selected,
            );
        }
        DetailRow::ListOfMapsDiffForcesReplacement { key, block } => {
            render_list_of_maps_diff(
                rows,
                key,
                block,
                ListOfMapsDiffHeader::ForcesReplacement,
                is_selected,
            );
        }
        DetailRow::ForceReplaceMapHeader { key }
        | DetailRow::ForceReplaceListOfMapsHeader { key } => {
            rows.push(
                Row::new(
                    Indent::new(2),
                    vec![
                        Span::raw(format!("{}: ", key)),
                        Span::styled("(forces replacement)", Style::default().fg(Color::Magenta)),
                    ],
                )
                .highlighted(is_selected),
            );
        }
        DetailRow::Removed { key, old } => {
            let mut spans = vec![Span::styled(
                format!("{}: ", key),
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::CROSSED_OUT),
            )];
            spans.extend(value_spans_styled(old, ValueStyle::RedStrikethrough));
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::Default { key, value } => {
            let mut spans = vec![Span::styled(format!("{}: ", key), dim_style)];
            spans.extend(value_spans_dimmed(value));
            spans.push(Span::styled("  # default", dim_style));
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::ReadOnly { key } => {
            rows.push(
                Row::new(
                    Indent::new(2),
                    vec![
                        Span::styled(format!("{}: ", key), dim_style),
                        Span::styled("(known after apply)", dim_style),
                    ],
                )
                .highlighted(is_selected),
            );
        }
        DetailRow::HiddenUnchanged { count } => {
            rows.push(
                Row::new(
                    Indent::new(2),
                    vec![Span::styled(
                        hidden_unchanged_summary(*count, "attribute"),
                        dim_style,
                    )],
                )
                .highlighted(is_selected),
            );
        }
        DetailRow::ReplaceRemoved { key, old } => {
            let mut spans = vec![Span::raw(format!("{}: ", key))];
            spans.extend(value_spans_styled(old, ValueStyle::RedStrikethrough));
            spans.extend([
                Span::raw(" -> "),
                Span::styled(
                    "(removed)",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
            ]);
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::ReplaceCascade { key, old, new } => {
            let mut spans = vec![Span::raw(format!("{}: ", key))];
            spans.extend(value_spans_styled(old, ValueStyle::RedStrikethrough));
            spans.push(Span::raw(" -> "));
            spans.extend(value_spans_styled(new, ValueStyle::Yellow));
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
        DetailRow::TemporaryNameNote {
            can_rename,
            temporary_value,
            original_value,
            attribute,
        } => {
            let spans = if *can_rename {
                vec![Span::styled(
                    format!(
                        "# New resource created with {attribute} = {temporary_value}, \
                         then renamed to {original_value} after old resource is deleted"
                    ),
                    dim_style,
                )]
            } else {
                vec![Span::styled(
                    format!(
                        "# {attribute} = {temporary_value} (temporary); \
                         original {original_value} reused after old resource deleted"
                    ),
                    dim_style,
                )]
            };
            rows.push(Row::new(Indent::new(2), spans).highlighted(is_selected));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::detail_rows::{ListOfMapsDiffItem, MapExpandedEntry, NonEmptyListOfMapsBlock};
    use carina_core::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
    use carina_core::value::{PrettyValue, RenderedKey};
    use indexmap::IndexMap;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Paragraph;

    use crate::ui::test_render::{
        first_content_cell, into_lines, line_text, render_with_unstyled_indentation,
    };

    fn render_detail_row_to_buffer(row: &DetailRow, is_selected: bool) -> Buffer {
        let mut rows = Vec::new();
        render_detail_row_to_rows(&mut rows, row, is_selected);
        let lines = into_lines(rows);
        let height = u16::try_from(lines.len()).expect("rendered lines fit in terminal height");
        let mut terminal = Terminal::new(TestBackend::new(120, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn assert_row_background(buffer: &Buffer, row: u16, expected_text: &str) {
        let width = expected_text.chars().count();
        let actual_text = (0..width)
            .map(|column| buffer[(column as u16, row)].symbol())
            .collect::<String>();
        assert_eq!(actual_text, expected_text);

        for column in 0..width {
            assert_eq!(
                buffer[(column as u16, row)].bg,
                Color::DarkGray,
                "cell at row {row}, column {column} is not highlighted"
            );
        }
    }

    #[test]
    fn removed_row_keeps_indentation_unstyled() {
        let row = DetailRow::Removed {
            key: "legacy".to_string(),
            old: carina_core::value::render_value(&Value::Concrete(ConcreteValue::String(
                "gone".into(),
            ))),
        };
        let mut rows = Vec::new();

        render_detail_row_to_rows(&mut rows, &row, false);

        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let (column, key_cell) =
            first_content_cell(&buffer, 0).expect("removed row has key content");
        assert_eq!(column, 2, "removed row uses a two-column indent");
        assert_eq!(key_cell.fg, Color::Red);
        assert!(key_cell.modifier.contains(Modifier::CROSSED_OUT));
    }

    /// A multi-element list-of-maps entry inside a `MapExpanded` row
    /// must be followed by a blank `Line` before the next sibling
    /// entry, mirroring the CLI behavior added in #2555. A `*` marker
    /// alone is not enough to delimit the *end* of the list.
    #[test]
    fn map_expanded_inserts_blank_after_multi_element_list_of_maps() {
        let stmt = |sid: &str| {
            let mut m = IndexMap::new();
            m.insert(
                "sid".to_string(),
                Value::Concrete(ConcreteValue::String(sid.to_string().into())),
            );
            Value::Concrete(ConcreteValue::Map(m))
        };
        let row = DetailRow::MapExpanded {
            key: "policy_document".to_string(),
            entries: vec![
                MapExpandedEntry {
                    key: RenderedKey::from_raw("statement"),
                    value: PrettyValue::new(Value::Concrete(ConcreteValue::List(vec![
                        stmt("A"),
                        stmt("B"),
                    ]))),
                    annotation: None,
                },
                MapExpandedEntry {
                    key: RenderedKey::from_raw("version"),
                    value: PrettyValue::new(Value::Concrete(ConcreteValue::String(
                        "2012-10-17".into(),
                    ))),
                    annotation: None,
                },
            ],
        };

        let mut rows = Vec::new();
        render_detail_row_to_rows(&mut rows, &row, false);
        let lines = into_lines(rows);

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
            m.insert(
                "sid".to_string(),
                Value::Concrete(ConcreteValue::String(sid.to_string().into())),
            );
            Value::Concrete(ConcreteValue::Map(m))
        };
        let row = DetailRow::MapExpanded {
            key: "policy_document".to_string(),
            entries: vec![
                MapExpandedEntry {
                    key: RenderedKey::from_raw("version"),
                    value: PrettyValue::new(Value::Concrete(ConcreteValue::String(
                        "2012-10-17".into(),
                    ))),
                    annotation: None,
                },
                MapExpandedEntry {
                    key: RenderedKey::from_raw("statement"),
                    value: PrettyValue::new(Value::Concrete(ConcreteValue::List(vec![
                        stmt("A"),
                        stmt("B"),
                    ]))),
                    annotation: None,
                },
            ],
        };

        let mut rows = Vec::new();
        render_detail_row_to_rows(&mut rows, &row, false);
        let lines = into_lines(rows);

        let last = lines.last().expect("at least one line");
        assert!(
            !line_text(last).trim().is_empty(),
            "trailing list-of-maps must not leave an orphan blank line: lines={lines:?}",
        );
    }

    #[test]
    fn forcing_changed_renders_new_value_yellow() {
        let row = DetailRow::ChangedForcesReplacement {
            key: "name".to_string(),
            old: carina_core::value::render_value(&Value::Concrete(ConcreteValue::String(
                "old".into(),
            ))),
            new: carina_core::value::render_value(&Value::Concrete(ConcreteValue::String(
                "new".into(),
            ))),
        };

        let mut rows = Vec::new();
        render_detail_row_to_rows(&mut rows, &row, false);
        let lines = into_lines(rows);

        let line = lines.first().expect("changed row should render one line");
        let new_span = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "'new'")
            .expect("new value span should be present");
        assert_eq!(
            new_span.style,
            Style::default().fg(Color::Yellow),
            "forcing Changed rows must use the forcing yellow value style"
        );
    }

    #[test]
    fn forcing_list_of_maps_suffix_renders_before_open_bracket() {
        let row = DetailRow::ListOfMapsDiffForcesReplacement {
            key: "rules".to_string(),
            block: NonEmptyListOfMapsBlock::from_parts(
                vec![],
                vec![],
                vec![],
                vec![ListOfMapsDiffItem {
                    fields: vec![(
                        RenderedKey::from_raw("name"),
                        PrettyValue::new(Value::Concrete(ConcreteValue::String("legacy".into()))),
                    )],
                }],
            )
            .unwrap(),
        };

        let mut rows = Vec::new();
        render_detail_row_to_rows(&mut rows, &row, false);
        let lines = into_lines(rows);

        let header = lines.first().expect("list-of-maps header should render");
        assert_eq!(
            line_text(header),
            "  rules: (forces replacement) [",
            "forcing suffix must appear before the list opening bracket"
        );
    }

    #[test]
    fn selected_rows_highlight_indent_and_content() {
        let forcing_list = DetailRow::ListOfMapsDiffForcesReplacement {
            key: "key".to_string(),
            block: NonEmptyListOfMapsBlock::from_parts(
                vec![],
                vec![],
                vec![],
                vec![ListOfMapsDiffItem {
                    fields: vec![(
                        RenderedKey::from_raw("name"),
                        PrettyValue::new(Value::Concrete(ConcreteValue::String("legacy".into()))),
                    )],
                }],
            )
            .unwrap(),
        };
        let buffer = render_detail_row_to_buffer(&forcing_list, true);
        assert_row_background(&buffer, 0, "  key: (forces replacement) [");

        let attribute = DetailRow::Attribute {
            key: "name".to_string(),
            value: carina_core::value::render_value(&Value::Concrete(ConcreteValue::String(
                "plain".into(),
            ))),
            ref_binding: None,
            annotation: None,
        };
        let buffer = render_detail_row_to_buffer(&attribute, true);
        assert_row_background(&buffer, 0, "  name: 'plain'");
    }
}
