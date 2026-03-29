//! UI rendering for the TUI module info viewer

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::module_info_app::{FocusedPanel, ModuleInfoApp, SectionKind};

/// Draw the main layout: tree (70%), detail panel (30%), help bar (1 line)
pub fn draw(frame: &mut Frame, app: &mut ModuleInfoApp) {
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
    draw_help(frame, chunks[2]);
}

/// Draw the tree view with sections and entries
fn draw_tree(frame: &mut Frame, app: &mut ModuleInfoApp, area: Rect) {
    let kind_label = if app.is_module { "Module" } else { "File" };
    let title = format!(" {} Info: {} ", kind_label, app.title);
    let border_style = if app.focused_panel == FocusedPanel::Tree {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let is_selected = idx == app.selected;
            build_list_item(row, is_selected)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

/// Build a ListItem for a row
fn build_list_item(row: &crate::module_info_app::InfoRow, is_selected: bool) -> ListItem<'static> {
    let indent = "  ".repeat(row.depth);

    let spans = match row.kind {
        SectionKind::Header => {
            let mut s = vec![
                Span::raw(indent),
                Span::styled(
                    format!("=== {} ===", row.label),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if !row.detail.is_empty() {
                s.push(Span::styled(
                    format!("  ({})", row.detail),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            s
        }
        SectionKind::Argument => {
            let mut s = vec![
                Span::raw(indent.clone()),
                Span::styled(row.label.clone(), Style::default().fg(Color::White)),
                Span::raw(": "),
                Span::styled(row.type_info.clone(), Style::default().fg(Color::Yellow)),
            ];
            if row.detail.contains("required") {
                s.push(Span::styled(
                    "  (required)",
                    Style::default().fg(Color::Red),
                ));
            }
            if row.detail.contains("default:")
                && let Some(default_part) = row.detail.split("default: ").nth(1)
            {
                let default_val = default_part.split(" | ").next().unwrap_or(default_part);
                s.push(Span::styled(
                    format!(" = {}", default_val),
                    Style::default().fg(Color::Green),
                ));
            }
            s
        }
        SectionKind::Resource => {
            vec![
                Span::raw(indent),
                Span::styled(row.label.clone(), Style::default().fg(Color::White)),
                Span::raw(": "),
                Span::styled(row.type_info.clone(), Style::default().fg(Color::Yellow)),
            ]
        }
        SectionKind::Attribute => {
            let mut s = vec![
                Span::raw(indent),
                Span::styled(row.label.clone(), Style::default().fg(Color::White)),
            ];
            if !row.type_info.is_empty() {
                s.push(Span::raw(": "));
                s.push(Span::styled(
                    row.type_info.clone(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            s
        }
        SectionKind::Import => {
            vec![
                Span::raw(indent),
                Span::styled(row.label.clone(), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("  {}", row.detail),
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        }
        SectionKind::ModuleCall => {
            vec![
                Span::raw(indent),
                Span::styled(row.label.clone(), Style::default().fg(Color::Blue)),
            ]
        }
    };

    let mut line_spans = spans;
    if is_selected {
        line_spans = line_spans
            .into_iter()
            .map(|s| {
                let mut style = s.style;
                style = style.add_modifier(Modifier::BOLD);
                Span::styled(s.content, style)
            })
            .collect();
    }

    ListItem::new(Line::from(line_spans))
}

/// Draw the detail panel
fn draw_detail(frame: &mut Frame, app: &ModuleInfoApp, area: Rect) {
    let border_style = if app.focused_panel == FocusedPanel::Detail {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let detail_text = if app.detail_lines.is_empty() {
        "(no details)".to_string()
    } else {
        app.detail_lines.join("\n")
    };

    let paragraph = Paragraph::new(detail_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(" Detail "),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Draw the help bar
fn draw_help(frame: &mut Frame, area: Rect) {
    let help = Paragraph::new(Line::from(vec![
        Span::styled(" q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit  "),
        Span::styled("↑↓/jk", Style::default().fg(Color::Yellow)),
        Span::raw(" navigate  "),
        Span::styled("Tab", Style::default().fg(Color::Yellow)),
        Span::raw(" switch panel"),
    ]));
    frame.render_widget(help, area);
}
