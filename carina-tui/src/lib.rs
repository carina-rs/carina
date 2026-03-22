//! TUI (Terminal User Interface) for interactive plan review
//!
//! Provides an interactive tree view of a Plan with color-coded effects
//! and an attribute detail panel.

mod app;
mod ui;

#[cfg(test)]
mod tui_snapshot_tests;

use std::collections::HashMap;
use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{ExecutableCommand, execute};
use ratatui::prelude::*;

use carina_core::plan::Plan;
use carina_core::schema::ResourceSchema;

pub use app::{App, FocusedPanel};

/// Run the TUI with the given plan and optional schemas.
///
/// When schemas are provided, the detail panel shows read-only attributes
/// with `(known after apply)` and default values with `# default`,
/// matching CLI `--detail full` behavior.
///
/// Takes ownership of the terminal, displays the interactive plan viewer,
/// and restores the terminal on exit.
pub fn run(plan: &Plan, schemas: &HashMap<String, ResourceSchema>) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(plan, schemas);
    let result = run_loop(&mut terminal, &mut app);

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Tab => app.toggle_focus(),
                KeyCode::Up | KeyCode::Char('k') => match app.focused_panel {
                    FocusedPanel::Tree => app.move_up(),
                    FocusedPanel::Detail => app.detail_scroll_up(),
                },
                KeyCode::Down | KeyCode::Char('j') => match app.focused_panel {
                    FocusedPanel::Tree => app.move_down(),
                    FocusedPanel::Detail => app.detail_scroll_down(),
                },
                _ => {}
            }
        }
    }
}
