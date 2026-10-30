//! Map and list-of-maps diff rendering.

use carina_core::detail_rows::{
    ListOfMapsDiffField, ListOfMapsDiffItem, ListOfMapsDiffItemKind, MapDiffEntryIR,
    NonEmptyListOfMapsBlock, hidden_unchanged_summary,
};
use carina_core::value::{PrettyLayout, RenderedValue};
use ratatui::prelude::*;

use crate::row::{Indent, Row};

use super::push_annotation_span;
use super::value_view::{ValueStyle, value_lines_styled, value_spans_styled};

pub(super) enum ListOfMapsDiffHeader {
    Standard,
    ForcesReplacement,
}

/// Render string-list diff entries (#2943) into TUI lines. Mirrors
/// `carina-cli::display::render_string_list_diff_entries` — including
/// the trailing-summary placement.
pub(super) fn render_string_list_diff_entries(
    lines: &mut Vec<Row<'_>>,
    unchanged: &[RenderedValue],
    added: &[RenderedValue],
    removed: &[RenderedValue],
) {
    for s in removed {
        let mut spans = vec![Span::styled(
            "- ",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::CROSSED_OUT),
        )];
        spans.extend(value_spans_styled(s, ValueStyle::RedStrikethrough));
        lines.push(Row::new(Indent::new(4), spans));
    }
    for s in added {
        let mut spans = vec![Span::styled("+ ", Style::default().fg(Color::Green))];
        spans.extend(value_spans_styled(s, ValueStyle::Green));
        lines.push(Row::new(Indent::new(4), spans));
    }
    if !unchanged.is_empty() {
        lines.push(Row::new(
            Indent::new(4),
            vec![Span::styled(
                hidden_unchanged_summary(unchanged.len(), "element"),
                Style::default().fg(Color::DarkGray),
            )],
        ));
    }
}

/// Render map diff entries into TUI lines.
pub(super) fn render_map_diff_entries(lines: &mut Vec<Row<'_>>, entries: &[MapDiffEntryIR]) {
    for entry in entries {
        match entry {
            MapDiffEntryIR::Changed {
                key,
                old,
                new,
                annotation,
            } => {
                let mut spans = vec![
                    Span::styled("~ ", Style::default().fg(Color::Yellow)),
                    Span::raw(format!("{}: ", key)),
                ];
                spans.extend(value_spans_styled(old, ValueStyle::RedStrikethrough));
                spans.push(Span::raw(" -> "));
                spans.extend(value_spans_styled(new, ValueStyle::Green));
                push_annotation_span(&mut spans, annotation.as_deref());
                lines.push(Row::new(Indent::new(4), spans));
            }
            MapDiffEntryIR::Added {
                key,
                value,
                annotation,
            } => {
                let mut spans = vec![
                    Span::styled("+ ", Style::default().fg(Color::Green)),
                    Span::raw(format!("{}: ", key)),
                ];
                spans.extend(value_spans_styled(value, ValueStyle::Green));
                push_annotation_span(&mut spans, annotation.as_deref());
                lines.push(Row::new(Indent::new(4), spans));
            }
            MapDiffEntryIR::Removed { key, value } => {
                let mut spans = vec![
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
                ];
                spans.extend(value_spans_styled(value, ValueStyle::RedStrikethrough));
                lines.push(Row::new(Indent::new(4), spans));
            }
            MapDiffEntryIR::NestedMapDiff { key, entries } => {
                lines.push(Row::new(
                    Indent::new(6),
                    vec![Span::raw(format!("{}:", key))],
                ));
                let mut nested_lines = Vec::new();
                render_map_diff_entries(&mut nested_lines, entries.as_slice());
                for line in nested_lines {
                    lines.push(line.indented(Indent::new(4)));
                }
            }
            MapDiffEntryIR::NestedListOfMapsDiff { key, block } => {
                let mut nested_lines = Vec::new();
                render_list_of_maps_diff(
                    &mut nested_lines,
                    key.plain(),
                    block,
                    ListOfMapsDiffHeader::Standard,
                    false,
                );
                for line in nested_lines {
                    lines.push(line.indented(Indent::new(4)));
                }
            }
            MapDiffEntryIR::StringListChanged {
                key,
                unchanged,
                added,
                removed,
            } => {
                // #3234.
                lines.push(Row::new(
                    Indent::new(6),
                    vec![Span::raw(format!("{}:", key))],
                ));
                let mut nested_lines = Vec::new();
                render_string_list_diff_entries(&mut nested_lines, unchanged, added, removed);
                for line in nested_lines {
                    lines.push(line.indented(Indent::new(4)));
                }
            }
        }
    }
}

/// Render list-of-maps diff into TUI lines.
pub(super) fn render_list_of_maps_diff(
    lines: &mut Vec<Row<'_>>,
    key: &str,
    block: &NonEmptyListOfMapsBlock,
    header: ListOfMapsDiffHeader,
    highlighted: bool,
) {
    let header_spans = match header {
        ListOfMapsDiffHeader::Standard => vec![Span::raw(format!("{}: [", key))],
        ListOfMapsDiffHeader::ForcesReplacement => {
            vec![
                Span::raw(format!("{}: ", key)),
                Span::styled("(forces replacement)", Style::default().fg(Color::Magenta)),
                Span::raw(" ["),
            ]
        }
    };
    lines.push(Row::new(Indent::new(2), header_spans).highlighted(highlighted));
    for item in block.unchanged() {
        lines.push(Row::new(
            Indent::new(4),
            value_spans_styled(item, ValueStyle::DarkGray),
        ));
    }
    for m in block.modified() {
        let mut spans = vec![Span::raw("{")];
        let mut spans_indent = Indent::new(4);
        let mut emitted = 0usize;
        for field in m.fields.iter() {
            if emitted > 0 {
                spans.push(Span::raw(", "));
            }
            emitted += 1;
            match field {
                ListOfMapsDiffField::Changed { key, old, new } => {
                    spans.push(Span::raw(format!("{}: ", key)));
                    spans.extend(value_spans_styled(old, ValueStyle::RedStrikethrough));
                    spans.push(Span::raw(" -> "));
                    spans.extend(value_spans_styled(new, ValueStyle::Green));
                }
                ListOfMapsDiffField::NestedMapChanged { key, entries } => {
                    // Flush current spans as a line, then render nested entries
                    spans.push(Span::raw(format!("{}: ", key)));
                    lines.push(Row::new(spans_indent, std::mem::take(&mut spans)));
                    spans_indent = Indent::new(0);
                    let mut nested_lines = Vec::new();
                    render_map_diff_entries(&mut nested_lines, entries);
                    for line in nested_lines {
                        lines.push(line.indented(Indent::new(6)));
                    }
                }
                ListOfMapsDiffField::StringListChanged {
                    key,
                    unchanged,
                    added,
                    removed,
                } => {
                    spans.push(Span::raw(format!("{}: ", key)));
                    lines.push(Row::new(spans_indent, std::mem::take(&mut spans)));
                    spans_indent = Indent::new(0);
                    let mut nested_lines = Vec::new();
                    render_string_list_diff_entries(&mut nested_lines, unchanged, added, removed);
                    for line in nested_lines {
                        lines.push(line.indented(Indent::new(6)));
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
                lines.push(Row::new(Indent::new(6), vec![Span::styled(summary, dim)]));
            } else {
                if emitted > 0 {
                    spans.push(Span::raw(", "));
                }
                spans.push(Span::styled(summary, dim));
            }
        }
        if !spans.is_empty() {
            spans.push(Span::raw("}"));
            lines.push(Row::new(spans_indent, spans));
        } else {
            // Closing brace on its own line aligned with the opening.
            lines.push(Row::new(Indent::new(4), vec![Span::raw("}")]));
        }
    }
    for item in block.added() {
        push_added_removed_block(lines, item, ListOfMapsDiffItemKind::Added);
    }
    for item in block.removed() {
        push_added_removed_block(lines, item, ListOfMapsDiffItemKind::Removed);
    }
    lines.push(Row::new(Indent::new(2), vec![Span::raw("]")]));
}

/// Render a wholly added or removed list-of-maps element as a multi-line
/// `+ {` / `- {` block (#2877). Mirrors the CLI display in
/// `carina-cli/src/display/mod.rs::render_added_removed_block`. Each
/// field's value goes through `PrettyValue::render` so nested long lists
/// or maps wrap to multiple indented lines instead of dumping inline.
fn push_added_removed_block(
    lines: &mut Vec<Row<'_>>,
    item: &ListOfMapsDiffItem,
    kind: ListOfMapsDiffItemKind,
) {
    let (marker, color, modifier) = match kind {
        ListOfMapsDiffItemKind::Added => ("+", Color::Green, Modifier::empty()),
        ListOfMapsDiffItemKind::Removed => ("-", Color::Red, Modifier::CROSSED_OUT),
    };
    let style = Style::default().fg(color).add_modifier(modifier);

    lines.push(Row::new(
        Indent::new(4),
        vec![Span::styled(format!("{} {{", marker), style)],
    ));

    // Constant indent; nesting is added by the caller via `Row::indented`.
    let field_indent_cols = 6;
    let mut prev_needs_separator = false;
    for (key, value) in &item.fields {
        // Mirror `format_map_vertical`: insert a blank line before the
        // next sibling key when the previous value was a multi-element
        // list-of-maps so the boundary stays visible (#2555).
        if prev_needs_separator {
            lines.push(Row::blank());
        }
        prev_needs_separator = value.needs_trailing_separator();
        let layout = PrettyLayout::for_map_key(field_indent_cols, key);
        let pretty = value.render(layout);
        let value_style = match kind {
            ListOfMapsDiffItemKind::Added => ValueStyle::Green,
            ListOfMapsDiffItemKind::Removed => ValueStyle::RedStrikethrough,
        };
        for (index, mut value_spans) in value_lines_styled(&pretty, value_style)
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                let mut spans = vec![Span::styled(format!("{}: ", key), style)];
                spans.append(&mut value_spans);
                lines.push(Row::new(Indent::new(field_indent_cols), spans));
            } else {
                lines.push(Row::new(Indent::new(0), value_spans));
            }
        }
    }

    lines.push(Row::new(
        Indent::new(4),
        vec![Span::styled("}".to_string(), style)],
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::detail_rows::{
        DetailLevel, DetailRow, HistoricalDisplayAttributes, NonEmptyListOfMapsBlock,
        build_detail_rows_by_instance_for_test,
    };
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::{ConcreteValue, Value};
    use carina_core::resource::{ResolvedResource, Resource, ResourceId, State};
    use carina_core::value::{PrettyValue, RenderedKey};
    use indexmap::IndexMap;

    use crate::ui::test_render::{
        first_content_cell, into_lines, leading_space_count, line_text,
        render_with_unstyled_indentation,
    };

    fn rendered_string(value: &str) -> RenderedValue {
        carina_core::value::render_value(&Value::Concrete(ConcreteValue::String(value.into())))
    }

    fn string_value(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    fn map_value(entries: Vec<(&str, Value)>) -> Value {
        Value::Concrete(ConcreteValue::Map(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect::<IndexMap<_, _>>(),
        ))
    }

    fn update_detail_rows(old: Value, new: Value) -> Vec<DetailRow> {
        let id = ResourceId::with_identity("test.Widget", "example");
        let effect = Effect::Update {
            identifier: crate::fixture_applied_identifier("example-id"),
            from: Box::new(
                State::existing(id, [("value".to_string(), old)].into_iter().collect())
                    .with_identifier("example-id"),
            ),
            to: ResolvedResource::new(
                Resource::new("test.Widget", "example").with_attribute("value", new),
            ),
            changed_attributes: vec!["value".to_string()],
        };
        let mut plan = Plan::new();
        plan.add(effect);
        let historical = HistoricalDisplayAttributes::for_plan(&plan);

        build_detail_rows_by_instance_for_test(
            &plan.effects()[0],
            None,
            DetailLevel::Explicit,
            &historical,
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
    }

    #[test]
    fn nested_map_diff_keeps_wrapper_and_inner_indentation_unstyled() {
        let old = map_value(vec![(
            "nested",
            map_value(vec![("removed", string_value("old"))]),
        )]);
        let new = map_value(vec![(
            "nested",
            map_value(vec![("added", string_value("new"))]),
        )]);
        let rows = update_detail_rows(old, new);
        let entries = match rows.as_slice() {
            [DetailRow::MapDiff { entries, .. }] => entries,
            other => panic!("expected one map-diff row, got {other:?}"),
        };
        let nested_entries = match entries.as_slice() {
            [MapDiffEntryIR::NestedMapDiff { entries, .. }] => entries.as_slice(),
            other => panic!("expected one nested map diff, got {other:?}"),
        };
        assert!(
            nested_entries
                .iter()
                .any(|entry| matches!(entry, MapDiffEntryIR::Removed { .. }))
        );
        assert!(
            nested_entries
                .iter()
                .any(|entry| matches!(entry, MapDiffEntryIR::Added { .. }))
        );

        let mut rows = Vec::new();
        render_map_diff_entries(&mut rows, entries.as_slice());
        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let mut saw_removed = false;
        let mut saw_added = false;

        for row in 0..buffer.area.height {
            let Some((column, cell)) = first_content_cell(&buffer, row) else {
                continue;
            };
            match cell.symbol() {
                "-" => {
                    assert_eq!(column, 8, "nested removed marker indentation");
                    assert_eq!(cell.fg, Color::Red);
                    assert!(cell.modifier.contains(Modifier::CROSSED_OUT));
                    saw_removed = true;
                }
                "+" => {
                    assert_eq!(column, 8, "nested added marker indentation");
                    assert_eq!(cell.fg, Color::Green);
                    assert!(!cell.modifier.contains(Modifier::CROSSED_OUT));
                    saw_added = true;
                }
                _ => {}
            }
        }

        assert!(saw_removed, "expected a styled nested removed marker");
        assert!(saw_added, "expected a styled nested added marker");
    }

    #[test]
    fn nested_list_of_maps_removed_multiline_value_keeps_indentation_unstyled() {
        let long_string_list = || {
            Value::Concrete(ConcreteValue::StringList(vec![
                "a".repeat(60).into(),
                "b".repeat(60).into(),
                "c".repeat(60).into(),
            ]))
        };
        let item = ListOfMapsDiffItem {
            fields: vec![
                (
                    RenderedKey::from_raw("cfg"),
                    PrettyValue::new(map_value(vec![
                        ("first", long_string_list()),
                        ("second", long_string_list()),
                    ])),
                ),
                (
                    RenderedKey::from_raw("values"),
                    PrettyValue::new(long_string_list()),
                ),
            ],
        };
        let block = NonEmptyListOfMapsBlock::from_parts(vec![], vec![], vec![], vec![item])
            .expect("removed item makes the block non-empty");
        let entries = vec![MapDiffEntryIR::NestedListOfMapsDiff {
            key: RenderedKey::from_raw("rules"),
            block,
        }];
        let mut rows = Vec::new();

        render_map_diff_entries(&mut rows, &entries);

        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let mut saw_removed_marker = false;
        let mut saw_field_row = false;
        let mut saw_deep_indentation = false;
        for row in 0..buffer.area.height {
            let indent = leading_space_count(&buffer, row);
            saw_deep_indentation |= indent >= 12;
            let Some((column, cell)) = first_content_cell(&buffer, row) else {
                continue;
            };

            if cell.symbol() == "-" {
                assert_eq!(column, 8, "nested removed block marker indentation");
                assert_eq!(cell.fg, Color::Red);
                assert!(cell.modifier.contains(Modifier::CROSSED_OUT));
                saw_removed_marker = true;
            }
            if column >= 10 {
                assert_eq!(cell.fg, Color::Red);
                assert!(
                    cell.modifier.contains(Modifier::CROSSED_OUT),
                    "first content cell is not crossed out at row {row}, column {column}"
                );
                saw_field_row = true;
            }
        }

        assert!(saw_removed_marker, "expected a styled removed marker");
        assert!(saw_field_row, "expected at least one removed field row");
        assert!(
            saw_deep_indentation,
            "expected a continuation row indented by at least 12 columns"
        );
    }

    #[test]
    fn modified_list_nested_map_keeps_wrapper_indentation_unstyled() {
        let old_item = map_value(vec![
            ("id", string_value("same")),
            (
                "settings",
                map_value(vec![
                    ("keep", string_value("same")),
                    ("removed", string_value("gone")),
                ]),
            ),
        ]);
        let new_item = map_value(vec![
            ("id", string_value("same")),
            ("settings", map_value(vec![("keep", string_value("same"))])),
        ]);
        let rows = update_detail_rows(
            Value::Concrete(ConcreteValue::List(vec![old_item])),
            Value::Concrete(ConcreteValue::List(vec![new_item])),
        );
        let block = match rows.as_slice() {
            [DetailRow::ListOfMapsDiff { block, .. }] => block,
            other => panic!("expected one list-of-maps diff row, got {other:?}"),
        };
        let modified = block.modified().first().expect("one modified item");
        assert!(matches!(
            modified.fields.as_slice(),
            [ListOfMapsDiffField::NestedMapChanged { entries, .. }]
                if entries.iter().any(|entry| matches!(entry, MapDiffEntryIR::Removed { .. }))
        ));
        let mut rows = Vec::new();

        render_list_of_maps_diff(
            &mut rows,
            "value",
            block,
            ListOfMapsDiffHeader::Standard,
            false,
        );

        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let mut saw_removed = false;
        for row in 0..buffer.area.height {
            let Some((column, cell)) = first_content_cell(&buffer, row) else {
                continue;
            };
            if cell.symbol() == "-" {
                assert_eq!(column, 10, "six-column wrapper plus map indentation");
                assert_eq!(cell.fg, Color::Red);
                assert!(cell.modifier.contains(Modifier::CROSSED_OUT));
                saw_removed = true;
            }
        }
        assert!(saw_removed, "expected a styled nested removed marker");
    }

    #[test]
    fn string_list_diff_keeps_indentation_unstyled() {
        let removed = [rendered_string("old")];
        let added = [rendered_string("new")];
        let mut rows = Vec::new();

        render_string_list_diff_entries(&mut rows, &[], &added, &removed);

        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let (removed_column, removed_cell) =
            first_content_cell(&buffer, 0).expect("removed entry content");
        assert_eq!(removed_column, 4);
        assert_eq!(removed_cell.symbol(), "-");
        assert_eq!(removed_cell.fg, Color::Red);
        assert!(removed_cell.modifier.contains(Modifier::CROSSED_OUT));

        let (added_column, added_cell) =
            first_content_cell(&buffer, 1).expect("added entry content");
        assert_eq!(added_column, 4);
        assert_eq!(added_cell.symbol(), "+");
        assert_eq!(added_cell.fg, Color::Green);
        assert!(!added_cell.modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn map_string_list_diff_wrapper_keeps_indentation_unstyled() {
        let entries = [MapDiffEntryIR::StringListChanged {
            key: RenderedKey::from_raw("values"),
            unchanged: Vec::new(),
            added: vec![rendered_string("new")],
            removed: vec![rendered_string("old")],
        }];
        let mut rows = Vec::new();

        render_map_diff_entries(&mut rows, &entries);

        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let (column, marker) = (0..buffer.area.height)
            .filter_map(|row| first_content_cell(&buffer, row))
            .find(|(_, cell)| cell.symbol() == "-")
            .expect("removed string-list element marker");
        assert_eq!(column, 8);
        assert_eq!(marker.fg, Color::Red);
        assert!(marker.modifier.contains(Modifier::CROSSED_OUT));
        assert!(
            (0..buffer.area.height)
                .filter_map(|row| first_content_cell(&buffer, row))
                .any(|(_, cell)| cell.symbol() == "+"),
            "added string-list element marker"
        );
    }

    #[test]
    fn modified_list_string_list_diff_wrapper_keeps_indentation_unstyled() {
        let old_item = map_value(vec![
            ("id", string_value("same")),
            (
                "values",
                Value::Concrete(ConcreteValue::StringList(vec!["keep".into(), "old".into()])),
            ),
        ]);
        let new_item = map_value(vec![
            ("id", string_value("same")),
            (
                "values",
                Value::Concrete(ConcreteValue::StringList(vec!["keep".into(), "new".into()])),
            ),
        ]);
        let detail_rows = update_detail_rows(
            Value::Concrete(ConcreteValue::List(vec![old_item])),
            Value::Concrete(ConcreteValue::List(vec![new_item])),
        );
        let block = match detail_rows.as_slice() {
            [DetailRow::ListOfMapsDiff { block, .. }] => block,
            other => panic!("expected one list-of-maps diff row, got {other:?}"),
        };
        let modified = block.modified().first().expect("one modified item");
        assert!(matches!(
            modified.fields.as_slice(),
            [ListOfMapsDiffField::StringListChanged { removed, .. }]
                if !removed.is_empty()
        ));
        let mut rows = Vec::new();

        render_list_of_maps_diff(
            &mut rows,
            "value",
            block,
            ListOfMapsDiffHeader::Standard,
            false,
        );

        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let (column, marker) = (0..buffer.area.height)
            .filter_map(|row| first_content_cell(&buffer, row))
            .find(|(_, cell)| cell.symbol() == "-")
            .expect("removed string-list element marker");
        assert_eq!(column, 10);
        assert_eq!(marker.fg, Color::Red);
        assert!(marker.modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn map_diff_renders_annotations_on_added_and_changed_entries() {
        let entries = vec![
            MapDiffEntryIR::Added {
                key: RenderedKey::from_raw("Team"),
                value: rendered_string("x"),
                annotation: Some("# default_tags".to_string()),
            },
            MapDiffEntryIR::Changed {
                key: RenderedKey::from_raw("ManagedBy"),
                old: rendered_string("old"),
                new: rendered_string("carina"),
                annotation: Some("# default_tags".to_string()),
            },
            MapDiffEntryIR::Removed {
                key: RenderedKey::from_raw("Legacy"),
                value: rendered_string("gone"),
            },
        ];
        let mut rows = Vec::new();

        render_map_diff_entries(&mut rows, &entries);
        let lines = into_lines(rows);

        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec![
                "    + Team: 'x'  # default_tags",
                "    ~ ManagedBy: 'old' -> 'carina'  # default_tags",
                "    - Legacy: 'gone'",
            ]
        );
    }

    /// Regression for #3116: #3894 keeps removed-value strikethrough off layout indentation.
    #[test]
    fn removed_multiline_values_do_not_strike_layout_indentation() {
        let long_string_list = || {
            ConcreteValue::StringList(vec![
                "a".repeat(60).into(),
                "b".repeat(60).into(),
                "c".repeat(60).into(),
            ])
        };
        let item = ListOfMapsDiffItem {
            fields: vec![
                (
                    RenderedKey::from_raw("cfg"),
                    PrettyValue::new(Value::Concrete(ConcreteValue::Map(
                        vec![
                            ("first".to_string(), Value::Concrete(long_string_list())),
                            ("second".to_string(), Value::Concrete(long_string_list())),
                        ]
                        .into_iter()
                        .collect(),
                    ))),
                ),
                (
                    RenderedKey::from_raw("values"),
                    PrettyValue::new(Value::Concrete(long_string_list())),
                ),
            ],
        };
        let block = NonEmptyListOfMapsBlock::from_parts(vec![], vec![], vec![], vec![item])
            .expect("removed item makes the block non-empty");
        let mut rows = Vec::new();

        render_list_of_maps_diff(
            &mut rows,
            "rules",
            &block,
            ListOfMapsDiffHeader::Standard,
            false,
        );
        let buffer = render_with_unstyled_indentation(into_lines(rows));
        let width = buffer.area.width;
        let height = buffer.area.height;
        let mut has_nested_continuation = false;

        for row in 0..height {
            let indent = leading_space_count(&buffer, row);
            if indent == usize::from(width) {
                continue;
            }

            has_nested_continuation |= indent >= 10;

            let last_content_column = (indent..usize::from(width))
                .rfind(|&column| buffer[(column as u16, row)].symbol() != " ")
                .expect("non-blank row has content");
            let row_text = (indent..=last_content_column)
                .map(|column| buffer[(column as u16, row)].symbol())
                .collect::<String>();
            let is_layout_row = matches!(row_text.as_str(), "rules: [" | "]");

            if !is_layout_row {
                for column in indent..=last_content_column {
                    let cell = &buffer[(column as u16, row)];
                    if cell.symbol() == " " {
                        continue;
                    }

                    assert!(
                        cell.modifier.contains(Modifier::CROSSED_OUT),
                        "content cell is not crossed out at row {row}, column {column}"
                    );
                }
            }
        }

        assert!(
            has_nested_continuation,
            "expected a wrapped continuation row indented by at least 10 columns"
        );
    }
}
