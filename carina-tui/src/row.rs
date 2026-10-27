//! Typed TUI rows with structurally unstyled indentation.

use ratatui::prelude::*;

#[derive(Clone, Copy)]
pub(crate) struct Indent(usize);

impl Indent {
    pub(crate) const fn new(cols: usize) -> Self {
        Self(cols)
    }
}

pub(crate) struct Row<'a> {
    indent: Indent,
    content: Vec<Span<'a>>,
    highlighted: bool,
}

impl<'a> Row<'a> {
    /// Creates a row, moving leading content spaces, whatever their style, into
    /// the unstyled indent. [`Self::into_line`] is the only way to obtain a
    /// ratatui [`Line`] from a `Row`.
    pub(crate) fn new(indent: Indent, content: impl Into<Vec<Span<'a>>>) -> Self {
        let content = content.into();
        let mut normalized = Vec::with_capacity(content.len());
        let mut leading_space_cols = 0;
        let mut at_start = true;

        for mut span in content {
            if at_start {
                let leading_spaces = span
                    .content
                    .bytes()
                    .take_while(|byte| *byte == b' ')
                    .count();
                leading_space_cols += leading_spaces;

                if leading_spaces == span.content.len() {
                    continue;
                }
                if leading_spaces > 0 {
                    span.content = span.content[leading_spaces..].to_string().into();
                }
                at_start = false;
            }
            normalized.push(span);
        }

        Self {
            indent: Indent(indent.0 + leading_space_cols),
            content: normalized,
            highlighted: false,
        }
    }

    pub(crate) fn blank() -> Self {
        Self::new(Indent::new(0), Vec::new())
    }

    pub(crate) fn indented(mut self, extra: Indent) -> Self {
        self.indent.0 += extra.0;
        self
    }

    pub(crate) fn highlighted(mut self, on: bool) -> Self {
        self.highlighted = on;
        self
    }

    pub(crate) fn into_line(mut self) -> Line<'a> {
        let mut spans = Vec::new();
        if self.indent.0 > 0 {
            spans.push(Span::raw(" ".repeat(self.indent.0)));
        }
        spans.append(&mut self.content);

        let line = Line::from(spans);
        if self.highlighted {
            line.style(Style::default().bg(Color::DarkGray))
        } else {
            line
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_moves_styled_leading_spaces_into_unstyled_indent() {
        let style = Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::CROSSED_OUT);

        let line = Row::new(Indent::new(2), vec![Span::styled("  key", style)]).into_line();

        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "    ");
        assert_eq!(line.spans[0].style, Style::default());
        assert_eq!(line.spans[1].content.as_ref(), "key");
        assert_eq!(line.spans[1].style, style);
    }

    #[test]
    fn new_peels_across_spans_and_drops_all_space_spans() {
        let discarded_style = Style::default().fg(Color::Red);
        let retained_style = Style::default().fg(Color::Green);

        let line = Row::new(
            Indent::new(1),
            vec![
                Span::styled("  ", discarded_style),
                Span::raw(" "),
                Span::styled("  key", retained_style),
            ],
        )
        .into_line();

        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "      ");
        assert_eq!(line.spans[0].style, Style::default());
        assert_eq!(line.spans[1].content.as_ref(), "key");
        assert_eq!(line.spans[1].style, retained_style);
    }

    #[test]
    fn indented_adds_columns() {
        let line = Row::new(Indent::new(2), vec![Span::raw("key")])
            .indented(Indent::new(3))
            .into_line();

        assert_eq!(line.spans[0].content.as_ref(), "     ");
        assert_eq!(line.spans[1].content.as_ref(), "key");
    }

    #[test]
    fn highlighted_applies_background_as_line_style() {
        let line = Row::new(Indent::new(0), vec![Span::raw("key")])
            .highlighted(true)
            .into_line();

        assert_eq!(line.style, Style::default().bg(Color::DarkGray));
    }
}
