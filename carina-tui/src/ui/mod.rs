//! UI rendering for the TUI plan viewer.
//!
//! Decomposed into per-pane submodules: `tree`, `detail`, `diff`,
//! `value_view`, `search`, `help`, and `style`. Only `draw` is public.

mod detail;
mod diff;
mod help;
mod search;
mod style;
mod tree;
mod value_view;

use ratatui::prelude::*;

use crate::app::App;

use self::detail::draw_detail;
use self::help::draw_help;
use self::search::draw_search_bar;
use self::tree::draw_tree;

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
