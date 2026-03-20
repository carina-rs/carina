//! TUI (Terminal User Interface) for interactive plan review
//!
//! Provides an interactive tree view of a Plan with color-coded effects
//! and an attribute detail panel.

mod app;
mod ui;

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{ExecutableCommand, execute};
use ratatui::prelude::*;

use carina_core::plan::Plan;

pub use app::App;

/// Run the TUI with the given plan.
///
/// Takes ownership of the terminal, displays the interactive plan viewer,
/// and restores the terminal on exit.
pub fn run(plan: &Plan) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(plan);
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
                KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                KeyCode::Down | KeyCode::Char('j') => app.move_down(),
                _ => {}
            }
        }
    }
}
