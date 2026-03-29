//! TUI (Terminal User Interface) for interactive plan review
//!
//! Provides an interactive tree view of a Plan with color-coded effects
//! and an attribute detail panel. Also provides a module info viewer
//! for interactive exploration of module signatures.

mod app;
pub mod module_info_app;
mod module_info_ui;
mod ui;

#[cfg(test)]
mod module_info_tui_snapshot_tests;
#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tui_snapshot_tests;

use std::collections::HashMap;
use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{ExecutableCommand, execute};
use ratatui::prelude::*;

use carina_core::module::FileSignature;
use carina_core::plan::Plan;
use carina_core::schema::ResourceSchema;

pub use app::{App, FocusedPanel};
pub use module_info_app::ModuleInfoApp;

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
                // Cancel search and restore full tree view
                let saved_node = app.selected_node_idx();
                app.search_active = false;
                app.search_query.clear();
                app.search_matches.clear();
                app.current_match = 0;
                // Restore selection to the same node in the unfiltered list
                app.restore_selection(saved_node);
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
            KeyCode::Tab => {
                app.tab_complete();
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
        KeyCode::Char('q') | KeyCode::Esc if !app.search_matches.is_empty() => {
            // Clear active search filter before quitting
            let saved_node = app.selected_node_idx();
            app.search_query.clear();
            app.search_matches.clear();
            app.current_match = 0;
            // Restore selection to the same node in the unfiltered list
            app.restore_selection(saved_node);
            KeyAction::Continue
        }
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
                FocusedPanel::Detail => app.detail_select_up(),
            }
            KeyAction::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            match app.focused_panel {
                FocusedPanel::Tree => app.move_down(),
                FocusedPanel::Detail => app.detail_select_down(),
            }
            KeyAction::Continue
        }
        KeyCode::Enter if app.focused_panel == FocusedPanel::Tree => {
            app.focused_panel = FocusedPanel::Detail;
            app.detail_selected = 0;
            KeyAction::Continue
        }
        KeyCode::Enter if app.focused_panel == FocusedPanel::Detail => {
            if let Some(binding) = app.selected_detail_ref_binding() {
                app.follow_ref(&binding);
            }
            KeyAction::Continue
        }
        KeyCode::Backspace if !app.search_active => {
            app.nav_back();
            KeyAction::Continue
        }
        _ => KeyAction::Continue,
    }
}

/// Handle a key code for the module info TUI.
pub fn handle_module_info_key(app: &mut ModuleInfoApp, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
        KeyCode::Tab => {
            app.toggle_focus();
            KeyAction::Continue
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_up();
            KeyAction::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_down();
            KeyAction::Continue
        }
        _ => KeyAction::Continue,
    }
}

/// Run the module info TUI with the given file signature.
pub fn run_module_info(signature: &FileSignature) -> io::Result<()> {
    let mut app = ModuleInfoApp::new(signature);
    run_tui(module_info_ui::draw, handle_module_info_key, &mut app)
}

/// Run the TUI with the given plan and optional schemas.
///
/// When schemas are provided, the detail panel shows read-only attributes
/// with `(known after apply)` and default values with `# default`,
/// matching CLI `--detail full` behavior.
pub fn run(plan: &Plan, schemas: &HashMap<String, ResourceSchema>) -> io::Result<()> {
    let mut app = App::new(plan, schemas);
    run_tui(ui::draw, handle_key, &mut app)
}

fn run_tui<A>(
    draw_fn: impl Fn(&mut ratatui::Frame, &mut A),
    key_fn: impl Fn(&mut A, KeyCode) -> KeyAction,
    app: &mut A,
) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = (|| {
        loop {
            terminal.draw(|frame| draw_fn(frame, app))?;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key_fn(app, key.code) == KeyAction::Quit {
                    return Ok(());
                }
            }
        }
    })();

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
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
            Resource::new("s3.bucket", "my-bucket").with_binding("bucket"),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc").with_binding("vpc"),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "my-subnet")
                .with_binding("subnet")
                .with_attribute(
                    "vpc_id",
                    carina_core::resource::Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                        field_path: vec![],
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

    #[test]
    fn tab_in_search_mode_completes() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('s'));
        handle_key(&mut app, KeyCode::Char('u'));
        handle_key(&mut app, KeyCode::Char('b'));

        // Tab should autocomplete "sub" — matches "ec2.subnet" (resource type)
        // and "subnet" (binding); sorted alphabetically, "ec2.subnet" first
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.search_query, "ec2.subnet");
        assert!(app.search_active);
    }

    #[test]
    fn tab_in_normal_mode_toggles_focus() {
        let mut app = make_search_app();
        assert_eq!(app.focused_panel, FocusedPanel::Tree);
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.focused_panel, FocusedPanel::Detail);
    }

    #[test]
    fn esc_cancels_search_restores_all_nodes() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('s'));
        handle_key(&mut app, KeyCode::Char('u'));
        handle_key(&mut app, KeyCode::Char('b'));

        // With search active, only matching + ancestors visible
        let filtered_count = app.visible_count();
        assert!(filtered_count < 3);

        // Esc restores full tree
        handle_key(&mut app, KeyCode::Esc);
        assert_eq!(app.visible_count(), 3);
        assert!(!app.search_active);
    }

    #[test]
    fn enter_confirms_search_keeps_filter() {
        let mut app = make_search_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('v'));
        handle_key(&mut app, KeyCode::Char('p'));
        handle_key(&mut app, KeyCode::Char('c'));
        handle_key(&mut app, KeyCode::Enter);

        // Filter should remain (query is not cleared)
        assert!(!app.search_active);
        assert_eq!(app.search_query, "vpc");
        assert!(!app.search_matches.is_empty());
    }

    fn make_provider_prefixed_app() -> App {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::with_provider("awscc", "ec2.vpc", "my-vpc").with_binding("vpc"),
        ));
        plan.add(Effect::Create(
            Resource::with_provider("awscc", "ec2.subnet", "my-subnet").with_binding("subnet"),
        ));
        plan.add(Effect::Create(
            Resource::with_provider("awscc", "s3.bucket", "my-bucket").with_binding("bucket"),
        ));
        App::new(&plan, &HashMap::new())
    }

    #[test]
    fn tab_complete_with_provider_prefix() {
        // When resource_type is "awscc.ec2.vpc", typing "ec" then Tab
        // should complete to the full resource type containing "ec"
        let mut app = make_provider_prefixed_app();
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('e'));
        handle_key(&mut app, KeyCode::Char('c'));

        handle_key(&mut app, KeyCode::Tab);
        // Should complete to a resource type containing "ec"
        assert!(
            app.search_query.contains("ec2"),
            "expected query to contain 'ec2', got '{}'",
            app.search_query
        );
        assert!(app.search_active);
    }

    #[test]
    fn esc_clears_filter_before_quitting() {
        let mut app = make_search_app();
        // Search for "vpc" and confirm with Enter
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('v'));
        handle_key(&mut app, KeyCode::Char('p'));
        handle_key(&mut app, KeyCode::Char('c'));
        handle_key(&mut app, KeyCode::Enter);

        // Filter is active (search_matches not empty)
        assert!(!app.search_matches.is_empty());
        assert!(!app.search_active);

        // First Esc should clear filter, not quit
        let action = handle_key(&mut app, KeyCode::Esc);
        assert_eq!(action, KeyAction::Continue);
        assert!(app.search_matches.is_empty());
        assert!(app.search_query.is_empty());
        // All nodes should be visible again
        assert_eq!(app.visible_count(), 3);

        // Second Esc should quit
        let action = handle_key(&mut app, KeyCode::Esc);
        assert_eq!(action, KeyAction::Quit);
    }

    #[test]
    fn q_clears_filter_before_quitting() {
        let mut app = make_search_app();
        // Search for "vpc" and confirm with Enter
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('v'));
        handle_key(&mut app, KeyCode::Char('p'));
        handle_key(&mut app, KeyCode::Char('c'));
        handle_key(&mut app, KeyCode::Enter);

        // Filter is active (search_matches not empty)
        assert!(!app.search_matches.is_empty());
        assert!(!app.search_active);

        // First q should clear filter, not quit
        let action = handle_key(&mut app, KeyCode::Char('q'));
        assert_eq!(action, KeyAction::Continue);
        assert!(app.search_matches.is_empty());
        assert!(app.search_query.is_empty());
        // All nodes should be visible again
        assert_eq!(app.visible_count(), 3);

        // Second q should quit
        let action = handle_key(&mut app, KeyCode::Char('q'));
        assert_eq!(action, KeyAction::Quit);
    }

    #[test]
    fn esc_quits_immediately_without_filter() {
        let mut app = make_search_app();
        // No search active, no filter
        assert!(app.search_matches.is_empty());
        let action = handle_key(&mut app, KeyCode::Esc);
        assert_eq!(action, KeyAction::Quit);
    }

    #[test]
    fn n_navigates_only_matching_nodes_in_filter_mode() {
        let mut app = make_search_app();
        // Search for "ec2" which matches vpc and subnet
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('e'));
        handle_key(&mut app, KeyCode::Char('c'));
        handle_key(&mut app, KeyCode::Char('2'));
        handle_key(&mut app, KeyCode::Enter);

        assert!(app.search_matches.len() >= 2);
        let first_match = app.search_matches[0];
        let second_match = app.search_matches[1];

        // Jump to first match
        assert_eq!(app.selected, first_match);

        // n -> next match (skips ancestor-only nodes)
        handle_key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.selected, second_match);
    }

    #[test]
    fn esc_in_search_preserves_selection() {
        let mut app = make_search_app();
        // Search for "sub" to filter to subnet
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('s'));
        handle_key(&mut app, KeyCode::Char('u'));
        handle_key(&mut app, KeyCode::Char('b'));

        // Select the subnet node in the filtered list
        let filtered_visible = app.visible_nodes();
        // Find subnet's position in the filtered list
        let subnet_pos = filtered_visible
            .iter()
            .position(|&idx| app.nodes[idx].effect_label.contains("subnet"))
            .expect("subnet should be visible");
        app.selected = subnet_pos;

        // Remember which absolute node was selected
        let subnet_node_idx = app.selected_node_idx().unwrap();

        // Esc clears the filter
        handle_key(&mut app, KeyCode::Esc);

        // After clearing, the same node should still be selected
        let new_node_idx = app.selected_node_idx().unwrap();
        assert_eq!(
            new_node_idx, subnet_node_idx,
            "selection should point to the same node after clearing filter"
        );
    }

    #[test]
    fn q_clear_filter_preserves_selection() {
        let mut app = make_search_app();
        // Search for "sub" and confirm with Enter
        handle_key(&mut app, KeyCode::Char('/'));
        handle_key(&mut app, KeyCode::Char('s'));
        handle_key(&mut app, KeyCode::Char('u'));
        handle_key(&mut app, KeyCode::Char('b'));
        handle_key(&mut app, KeyCode::Enter);

        // Navigate to subnet in the filtered list
        let filtered_visible = app.visible_nodes();
        let subnet_pos = filtered_visible
            .iter()
            .position(|&idx| app.nodes[idx].effect_label.contains("subnet"))
            .expect("subnet should be visible");
        app.selected = subnet_pos;
        let subnet_node_idx = app.selected_node_idx().unwrap();

        // q clears the filter (first press)
        handle_key(&mut app, KeyCode::Char('q'));

        // Selection should still point to subnet
        let new_node_idx = app.selected_node_idx().unwrap();
        assert_eq!(
            new_node_idx, subnet_node_idx,
            "selection should point to the same node after q clears filter"
        );
    }

    #[test]
    fn detail_select_up_down() {
        let mut app = make_search_app();
        // Navigate to subnet node which has 2 detail rows (vpc_id as ResourceRef)
        // Actually, subnet has only vpc_id. Let's find a node with multiple rows.
        // Navigate to subnet (ec2.subnet) - it has vpc_id attribute
        let visible = app.visible_nodes();
        let subnet_pos = visible
            .iter()
            .position(|&idx| app.nodes[idx].effect_label.contains("subnet"))
            .expect("subnet should be visible");
        app.selected = subnet_pos;

        // Verify it has at least 1 detail row
        let detail_row_count = app.selected_node().unwrap().detail_rows.len();
        assert!(detail_row_count > 0, "subnet should have detail rows");

        // Focus detail panel
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.focused_panel, FocusedPanel::Detail);
        assert_eq!(app.detail_selected, 0);

        // Should not go below 0
        handle_key(&mut app, KeyCode::Up);
        assert_eq!(app.detail_selected, 0);

        // If we have more than 1 row, test down
        if detail_row_count > 1 {
            handle_key(&mut app, KeyCode::Down);
            assert_eq!(app.detail_selected, 1);
            handle_key(&mut app, KeyCode::Up);
            assert_eq!(app.detail_selected, 0);
        }
    }

    #[test]
    fn detail_selected_resets_on_tree_navigation() {
        let mut app = make_search_app();
        // Navigate to subnet which has a detail row
        let visible = app.visible_nodes();
        let subnet_pos = visible
            .iter()
            .position(|&idx| app.nodes[idx].effect_label.contains("subnet"))
            .expect("subnet should be visible");
        app.selected = subnet_pos;

        // Focus detail
        handle_key(&mut app, KeyCode::Tab);
        // detail_selected starts at 0, that's fine

        // Switch to tree panel and navigate
        handle_key(&mut app, KeyCode::Tab);
        handle_key(&mut app, KeyCode::Down);

        // detail_selected should reset
        assert_eq!(app.detail_selected, 0);
    }

    #[test]
    fn enter_follows_ref_in_detail_panel() {
        let mut app = make_search_app();
        // Navigate to subnet (node index 2 in the visible list)
        // The tree is: vpc (0), subnet (1 - child of vpc), bucket (2 - root)
        // Actually let's find subnet
        let visible = app.visible_nodes();
        let subnet_pos = visible
            .iter()
            .position(|&idx| app.nodes[idx].effect_label.contains("subnet"))
            .expect("subnet should be visible");
        app.selected = subnet_pos;
        app.sync_list_state_pub();

        // Focus detail panel
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.focused_panel, FocusedPanel::Detail);

        // The subnet has vpc_id: vpc.vpc_id as first attribute
        // Check that it's navigable
        let ref_binding = app.selected_detail_ref_binding();
        assert_eq!(ref_binding, Some("vpc".to_string()));

        // Press Enter to follow the ref
        handle_key(&mut app, KeyCode::Enter);

        // Should now be on the vpc node
        let current_node = app.selected_node().unwrap();
        assert!(
            current_node.effect_label.contains("vpc"),
            "should be on vpc, got '{}'",
            current_node.effect_label
        );

        // Nav stack should have the subnet node
        assert_eq!(app.nav_stack.len(), 1);
    }

    #[test]
    fn backspace_navigates_back() {
        let mut app = make_search_app();
        // Navigate to subnet
        let visible = app.visible_nodes();
        let subnet_pos = visible
            .iter()
            .position(|&idx| app.nodes[idx].effect_label.contains("subnet"))
            .expect("subnet should be visible");
        app.selected = subnet_pos;
        app.sync_list_state_pub();
        let subnet_node_idx = app.selected_node_idx().unwrap();

        // Focus detail and follow ref
        handle_key(&mut app, KeyCode::Tab);
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.nav_stack.len(), 1);

        // Press Backspace to go back
        handle_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.nav_stack.len(), 0);

        // Should be back on subnet
        let current_idx = app.selected_node_idx().unwrap();
        assert_eq!(current_idx, subnet_node_idx);
    }

    #[test]
    fn nav_stack_supports_multiple_jumps() {
        let mut app = make_search_app();
        // Navigate to subnet
        let visible = app.visible_nodes();
        let subnet_pos = visible
            .iter()
            .position(|&idx| app.nodes[idx].effect_label.contains("subnet"))
            .expect("subnet should be visible");
        app.selected = subnet_pos;
        app.sync_list_state_pub();
        let subnet_node_idx = app.selected_node_idx().unwrap();

        // Follow ref from subnet -> vpc
        handle_key(&mut app, KeyCode::Tab);
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.nav_stack.len(), 1);

        // Back to subnet
        handle_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.nav_stack.len(), 0);
        assert_eq!(app.selected_node_idx().unwrap(), subnet_node_idx);

        // Backspace with empty stack does nothing
        handle_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.nav_stack.len(), 0);
    }

    #[test]
    fn enter_on_non_ref_attribute_does_nothing() {
        let mut app = make_search_app();
        // Select vpc node (has cidr_block attribute, not a ref)
        // vpc should be the first visible node
        app.selected = 0;

        // Focus detail panel
        handle_key(&mut app, KeyCode::Tab);

        // Should not be navigable (cidr_block is a string, not a ref)
        assert!(app.selected_detail_ref_binding().is_none());

        // Press Enter should do nothing
        let prev_selected = app.selected;
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected, prev_selected);
        assert!(app.nav_stack.is_empty());
    }

    // ---- Module info key handler tests ----

    fn make_module_info_app() -> ModuleInfoApp {
        use carina_core::parser::{ProviderContext, parse};
        let input = r#"
            arguments {
                vpc: aws.vpc
                enable_https: bool = true
            }
            attributes {
                sg: aws.security_group = web_sg.id
            }
            let web_sg = aws.security_group {
                name = "web-sg"
                vpc_id = vpc
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let sig = FileSignature::from_parsed_file_with_name(&parsed, "test_module");
        ModuleInfoApp::new(&sig)
    }

    #[test]
    fn module_info_q_quits() {
        let mut app = make_module_info_app();
        assert_eq!(
            handle_module_info_key(&mut app, KeyCode::Char('q')),
            KeyAction::Quit
        );
    }

    #[test]
    fn module_info_esc_quits() {
        let mut app = make_module_info_app();
        assert_eq!(
            handle_module_info_key(&mut app, KeyCode::Esc),
            KeyAction::Quit
        );
    }

    #[test]
    fn module_info_navigation() {
        let mut app = make_module_info_app();
        assert_eq!(app.selected(), 0);
        handle_module_info_key(&mut app, KeyCode::Down);
        assert_eq!(app.selected(), 1);
        handle_module_info_key(&mut app, KeyCode::Up);
        assert_eq!(app.selected(), 0);
        handle_module_info_key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected(), 1);
        handle_module_info_key(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn module_info_tab_toggles_focus() {
        let mut app = make_module_info_app();
        assert_eq!(app.focused_panel, FocusedPanel::Tree);
        handle_module_info_key(&mut app, KeyCode::Tab);
        assert_eq!(app.focused_panel, FocusedPanel::Detail);
        handle_module_info_key(&mut app, KeyCode::Tab);
        assert_eq!(app.focused_panel, FocusedPanel::Tree);
    }
}
