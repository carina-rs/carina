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
    // Search mode: capture input for the search query
    if app.search_active {
        match code {
            KeyCode::Esc => {
                // Cancel search
                app.search_active = false;
                app.search_query.clear();
                app.search_matches.clear();
                app.current_match = 0;
                return KeyAction::Continue;
            }
            KeyCode::Enter => {
                // Confirm search and jump to first match
                app.search_active = false;
                if !app.search_matches.is_empty() {
                    app.jump_to_current_match();
                }
                return KeyAction::Continue;
            }
            KeyCode::Backspace => {
                app.search_query.pop();
                app.update_search_matches();
                if !app.search_matches.is_empty() {
                    app.jump_to_current_match();
                }
                return KeyAction::Continue;
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
                app.update_search_matches();
                if !app.search_matches.is_empty() {
                    app.jump_to_current_match();
                }
                return KeyAction::Continue;
            }
            _ => return KeyAction::Continue,
        }
    }

    // Normal mode
    match code {
        KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
        KeyCode::Char('/') => {
            app.search_active = true;
            app.search_query.clear();
            app.search_matches.clear();
            app.current_match = 0;
            KeyAction::Continue
        }
        KeyCode::Char('n') if !app.search_matches.is_empty() => {
            app.next_match();
            KeyAction::Continue
        }
        KeyCode::Char('N') if !app.search_matches.is_empty() => {
            app.prev_match();
            KeyAction::Continue
        }
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

    fn make_search_app() -> App {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("s3.bucket", "my-bucket").with_attribute(
                "_binding",
                carina_core::resource::Value::String("bucket".to_string()),
            ),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc").with_attribute(
                "_binding",
                carina_core::resource::Value::String("vpc".to_string()),
            ),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "my-subnet")
                .with_attribute(
                    "_binding",
                    carina_core::resource::Value::String("subnet".to_string()),
                )
                .with_attribute(
                    "vpc_id",
                    carina_core::resource::Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                ),
        ));
        App::new(&plan, &HashMap::new())
    }

    #[test]
    fn slash_enters_search_mode() {
        let mut app = make_search_app();
        assert!(!app.search_active);
        handle_key(&mut app, KeyCode::Char('/'));
        assert!(app.search_active);
        assert!(app.search_query.is_empty());
    }

    #[test]
    fn search_typing_builds_query() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('v'));
        handle_key(&mut app, KeyCode::Char('p'));
        handle_key(&mut app, KeyCode::Char('c'));
        assert_eq!(app.search_query, "vpc");
        assert!(!app.search_matches.is_empty());
    }

    #[test]
    fn search_esc_cancels() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('v'));
        assert_eq!(handle_key(&mut app, KeyCode::Esc), KeyAction::Continue);
        assert!(!app.search_active);
        assert!(app.search_query.is_empty());
        assert!(app.search_matches.is_empty());
    }

    #[test]
    fn esc_in_search_mode_does_not_quit() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        let action = handle_key(&mut app, KeyCode::Esc);
        assert_eq!(action, KeyAction::Continue);
    }

    #[test]
    fn search_enter_confirms_and_jumps() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('s'));
        handle_key(&mut app, KeyCode::Char('u'));
        handle_key(&mut app, KeyCode::Char('b'));
        // "sub" should match "subnet"
        assert!(!app.search_matches.is_empty());
        let action = handle_key(&mut app, KeyCode::Enter);
        assert_eq!(action, KeyAction::Continue);
        assert!(!app.search_active);
        // Matches should remain for n/N navigation
        assert!(!app.search_matches.is_empty());
    }

    #[test]
    fn search_backspace_removes_char() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('v'));
        handle_key(&mut app, KeyCode::Char('p'));
        handle_key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.search_query, "vpx");
        handle_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.search_query, "vp");
    }

    #[test]
    fn n_and_shift_n_cycle_matches() {
        let mut app = make_search_app();
        // Search for "ec2" which should match vpc and subnet
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('e'));
        handle_key(&mut app, KeyCode::Char('c'));
        handle_key(&mut app, KeyCode::Char('2'));
        handle_key(&mut app, KeyCode::Enter);

        assert!(app.search_matches.len() >= 2);
        let first_selected = app.selected;

        // n -> next match
        handle_key(&mut app, KeyCode::Char('n'));
        let second_selected = app.selected;
        assert_ne!(first_selected, second_selected);

        // N -> previous match
        handle_key(&mut app, KeyCode::Char('N'));
        assert_eq!(app.selected, first_selected);
    }

    #[test]
    fn search_case_insensitive() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('V'));
        handle_key(&mut app, KeyCode::Char('P'));
        handle_key(&mut app, KeyCode::Char('C'));
        // Should still match "vpc" case-insensitively
        assert!(!app.search_matches.is_empty());
    }

    #[test]
    fn search_no_matches() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('z'));
        handle_key(&mut app, KeyCode::Char('z'));
        handle_key(&mut app, KeyCode::Char('z'));
        assert!(app.search_matches.is_empty());
    }

    #[test]
    fn n_without_matches_does_nothing() {
        let mut app = make_search_app();
        let initial_selected = app.selected;
        handle_key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.selected, initial_selected);
    }

    #[test]
    fn q_in_search_mode_appends_to_query() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        let action = handle_key(&mut app, KeyCode::Char('q'));
        assert_eq!(action, KeyAction::Continue);
        assert_eq!(app.search_query, "q");
        assert!(app.search_active);
    }
}
