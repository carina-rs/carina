//! Search input bar rendering.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::App;

/// Draw the search input bar at the bottom of the screen
pub(super) fn draw_search_bar(frame: &mut Frame, app: &App, area: Rect) {
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
