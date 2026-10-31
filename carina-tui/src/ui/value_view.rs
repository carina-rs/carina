//! Styling for semantically typed values rendered by `carina-core`.

use carina_core::value::{RenderedValue, ValueSpanKind};
use ratatui::prelude::*;

#[derive(Clone, Copy)]
pub(super) enum ValueStyle {
    Semantic,
    Dimmed,
    RedStrikethrough,
    Green,
    Yellow,
    DarkGray,
}

fn semantic_style(kind: ValueSpanKind) -> Style {
    match kind {
        ValueSpanKind::StringLiteral => Style::default().fg(Color::Green),
        ValueSpanKind::Bool => Style::default().fg(Color::Yellow),
        ValueSpanKind::Number => Style::default().fg(Color::White),
        ValueSpanKind::Enum => Style::default().fg(Color::Magenta),
        ValueSpanKind::Reference => Style::default().fg(Color::Cyan),
        ValueSpanKind::Structure
        | ValueSpanKind::Key
        | ValueSpanKind::Whitespace
        | ValueSpanKind::Other => Style::default(),
    }
}

fn span_style(kind: ValueSpanKind, style: ValueStyle) -> Style {
    if kind == ValueSpanKind::Whitespace {
        return Style::default();
    }
    match style {
        ValueStyle::Semantic => semantic_style(kind),
        ValueStyle::Dimmed => {
            let semantic = semantic_style(kind);
            if semantic.fg.is_some() {
                semantic.add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        }
        ValueStyle::RedStrikethrough => Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::CROSSED_OUT),
        ValueStyle::Green => Style::default().fg(Color::Green),
        ValueStyle::Yellow => Style::default().fg(Color::Yellow),
        ValueStyle::DarkGray => Style::default().fg(Color::DarkGray),
    }
}

pub(super) fn value_spans<'a>(rendered: &RenderedValue) -> Vec<Span<'a>> {
    value_spans_styled(rendered, ValueStyle::Semantic)
}

pub(super) fn value_spans_dimmed<'a>(rendered: &RenderedValue) -> Vec<Span<'a>> {
    value_spans_styled(rendered, ValueStyle::Dimmed)
}

pub(super) fn value_spans_styled<'a>(rendered: &RenderedValue, style: ValueStyle) -> Vec<Span<'a>> {
    rendered
        .spans()
        .iter()
        .map(|span| Span::styled(span.text().to_string(), span_style(span.kind(), style)))
        .collect()
}

pub(super) fn has_semantic_color(rendered: &RenderedValue) -> bool {
    rendered.spans().iter().any(|span| {
        matches!(
            span.kind(),
            ValueSpanKind::StringLiteral
                | ValueSpanKind::Bool
                | ValueSpanKind::Number
                | ValueSpanKind::Enum
                | ValueSpanKind::Reference
        )
    })
}

pub(super) fn value_lines_styled<'a>(
    rendered: &RenderedValue,
    style: ValueStyle,
) -> Vec<Vec<Span<'a>>> {
    let mut lines = vec![Vec::new()];
    for span in rendered.spans() {
        if span.kind() != ValueSpanKind::Whitespace || !span.text().contains('\n') {
            lines
                .last_mut()
                .expect("at least one line")
                .push(Span::styled(
                    span.text().to_string(),
                    span_style(span.kind(), style),
                ));
            continue;
        }

        let mut parts = span.text().split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                lines
                    .last_mut()
                    .expect("at least one line")
                    .push(Span::raw(part.to_string()));
            }
            if parts.peek().is_some() {
                lines.push(Vec::new());
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
    use carina_core::value::render_value;

    #[test]
    fn semantic_styles_follow_core_span_kinds() {
        let rendered = render_value(&Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("hello".into())),
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::enum_identifier("Mode.enabled")),
            carina_core::resource::AuthoredValue::resource_ref("server", "id", Vec::new())
                .resolve_root_for_test(),
        ])));
        let spans = value_spans(&rendered);

        assert!(spans.iter().any(|span| span.style.fg == Some(Color::Green)));
        assert!(
            spans
                .iter()
                .any(|span| span.style.fg == Some(Color::Yellow))
        );
        assert!(spans.iter().any(|span| span.style.fg == Some(Color::White)));
        assert!(
            spans
                .iter()
                .any(|span| span.style.fg == Some(Color::Magenta))
        );
        assert!(spans.iter().any(|span| span.style.fg == Some(Color::Cyan)));
        assert_eq!(
            spans.first().expect("opening bracket").style,
            Style::default()
        );
    }

    #[test]
    fn dimmed_semantic_styles_keep_their_colors() {
        let rendered = render_value(&Value::Concrete(ConcreteValue::String("hello".into())));
        let spans = value_spans_dimmed(&rendered);

        assert_eq!(spans[0].style.fg, Some(Color::Green));
        assert!(spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn override_style_does_not_style_layout_whitespace() {
        let rendered =
            carina_core::value::PrettyValue::new(Value::Concrete(ConcreteValue::StringList(vec![
                "a".repeat(70).into(),
                "b".repeat(70).into(),
            ])))
            .render(carina_core::value::PrettyLayout::for_attribute(4, "values"));
        let spans = value_spans_styled(&rendered, ValueStyle::RedStrikethrough);

        assert!(
            spans
                .iter()
                .any(|span| span.content.contains('\n') && span.style == Style::default())
        );
        assert!(
            spans
                .iter()
                .any(|span| { span.style.add_modifier.contains(Modifier::CROSSED_OUT) })
        );
    }

    #[test]
    fn styled_visual_lines_are_returned_as_span_vectors() {
        let rendered =
            carina_core::value::PrettyValue::new(Value::Concrete(ConcreteValue::StringList(vec![
                "a".repeat(70).into(),
                "b".repeat(70).into(),
            ])))
            .render(carina_core::value::PrettyLayout::for_attribute(4, "values"));

        let lines = value_lines_styled(&rendered, ValueStyle::RedStrikethrough);

        assert!(lines.len() > 1, "expected a wrapped value");
        assert!(
            lines
                .iter()
                .flatten()
                .any(|span| { span.style.add_modifier.contains(Modifier::CROSSED_OUT) })
        );
    }
}
