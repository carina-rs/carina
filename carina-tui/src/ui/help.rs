//! Bottom help-bar rendering.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{App, FocusedPanel};

/// Draw the help bar (search mode shows its own help via draw_search_bar)
pub(super) fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
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
