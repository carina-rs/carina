//! Plan title and effect-kind styling helpers.

use carina_core::plan::PlanSummary;
use ratatui::prelude::*;

use crate::app::EffectKind;

/// Build the plan title line with colored summary counts.
///
/// Matches CLI plan output colors: create=green, update=yellow, replace=magenta,
/// delete=red, read=cyan.
pub(super) fn build_plan_title(summary: &PlanSummary) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" Plan (Plan: ")];

    let mut parts_added = 0;

    if summary.read > 0 {
        if parts_added > 0 {
            spans.push(Span::raw(", "));
        }
        spans.push(Span::styled(
            format!("{}", summary.read),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw(" to read"));
        parts_added += 1;
    }

    // create is always shown
    if parts_added > 0 {
        spans.push(Span::raw(", "));
    }
    spans.push(Span::styled(
        format!("{}", summary.create),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::raw(" to create"));
    parts_added += 1;

    // update is always shown
    if parts_added > 0 {
        spans.push(Span::raw(", "));
    }
    spans.push(Span::styled(
        format!("{}", summary.update),
        Style::default().fg(Color::Yellow),
    ));
    spans.push(Span::raw(" to update"));
    parts_added += 1;

    if summary.replace > 0 {
        if parts_added > 0 {
            spans.push(Span::raw(", "));
        }
        spans.push(Span::styled(
            format!("{}", summary.replace),
            Style::default().fg(Color::Magenta),
        ));
        spans.push(Span::raw(" to replace"));
        parts_added += 1;
    }

    // delete is always shown
    if parts_added > 0 {
        spans.push(Span::raw(", "));
    }
    spans.push(Span::styled(
        format!("{}", summary.delete),
        Style::default().fg(Color::Red),
    ));
    spans.push(Span::raw(" to delete"));

    spans.push(Span::raw(") "));

    Line::from(spans)
}

/// Return the style for a given effect kind
pub(super) fn effect_style(kind: EffectKind) -> Style {
    match kind {
        EffectKind::Create => Style::default().fg(Color::Green),
        EffectKind::Update => Style::default().fg(Color::Yellow),
        EffectKind::Replace => Style::default().fg(Color::Yellow),
        EffectKind::Delete => Style::default().fg(Color::Red),
        EffectKind::Read => Style::default().fg(Color::Cyan),
    }
}
