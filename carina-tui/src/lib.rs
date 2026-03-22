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

/// Action resulting from a key press
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Quit the application
    Quit,
    /// Continue running (key was handled or ignored)
    Continue,
}

/// Handle a key code and apply the corresponding action to the app.
///
/// Returns `KeyAction::Quit` if the application should exit,
/// or `KeyAction::Continue` otherwise.
pub fn handle_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
        KeyCode::Tab => {
            app.toggle_focus();
            KeyAction::Continue
        }
        KeyCode::Up | KeyCode::Char('k') => {
            match app.focused_panel {
                FocusedPanel::Tree => app.move_up(),
                FocusedPanel::Detail => app.detail_scroll_up(),
            }
            KeyAction::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            match app.focused_panel {
                FocusedPanel::Tree => app.move_down(),
                FocusedPanel::Detail => app.detail_scroll_down(),
            }
            KeyAction::Continue
        }
        _ => KeyAction::Continue,
    }
}

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
            if handle_key(app, key.code) == KeyAction::Quit {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::effect::Effect;
    use carina_core::resource::Resource;

    fn make_app() -> App {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        plan.add(Effect::Create(Resource::new("s3.bucket", "b")));
        App::new(&plan, &HashMap::new())
    }

    #[test]
    fn q_key_triggers_quit() {
        let mut app = make_app();
        assert_eq!(handle_key(&mut app, KeyCode::Char('q')), KeyAction::Quit);
    }

    #[test]
    fn esc_key_triggers_quit() {
        let mut app = make_app();
        assert_eq!(handle_key(&mut app, KeyCode::Esc), KeyAction::Quit);
    }

    #[test]
    fn other_keys_continue() {
        let mut app = make_app();
        assert_eq!(
            handle_key(&mut app, KeyCode::Char('x')),
            KeyAction::Continue
        );
    }

    #[test]
    fn navigation_keys_continue() {
        let mut app = make_app();
        assert_eq!(handle_key(&mut app, KeyCode::Up), KeyAction::Continue);
        assert_eq!(handle_key(&mut app, KeyCode::Down), KeyAction::Continue);
        assert_eq!(
            handle_key(&mut app, KeyCode::Char('k')),
            KeyAction::Continue
        );
        assert_eq!(
            handle_key(&mut app, KeyCode::Char('j')),
            KeyAction::Continue
        );
        assert_eq!(handle_key(&mut app, KeyCode::Tab), KeyAction::Continue);
    }
}
