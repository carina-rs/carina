use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

const WIDTH: u16 = 120;

pub(super) fn render_with_unstyled_indentation(lines: Vec<Line<'_>>) -> Buffer {
    let height = u16::try_from(lines.len()).expect("rendered lines fit in terminal height");
    assert!(height > 0, "expected at least one rendered line");

    let mut terminal = Terminal::new(TestBackend::new(WIDTH, height)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();

    // `Cell::style()` materializes unset colors as `Color::Reset`, so the
    // rendered equivalent of `Style::default()` is a default cell's style.
    let default_style = Cell::default().style();
    for row in 0..height {
        let indent = leading_space_count(&buffer, row);
        for column in 0..indent {
            let cell = &buffer[(column as u16, row)];
            assert_eq!(
                cell.style(),
                default_style,
                "leading indentation is styled at row {row}, column {column}"
            );
        }
    }

    buffer
}

pub(super) fn leading_space_count(buffer: &Buffer, row: u16) -> usize {
    (0..buffer.area.width)
        .take_while(|&column| buffer[(column, row)].symbol() == " ")
        .count()
}

pub(super) fn first_content_cell(buffer: &Buffer, row: u16) -> Option<(u16, &Cell)> {
    (0..buffer.area.width)
        .find(|&column| buffer[(column, row)].symbol() != " ")
        .map(|column| (column, &buffer[(column, row)]))
}
