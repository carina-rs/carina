//! Plan title and effect-kind styling helpers.

use carina_core::plan::{DeferredSummaryAction, PlanSummary, PlanSummaryPart};
use ratatui::prelude::*;

use crate::app::EffectKind;

/// Build the plan title line with colored summary counts.
///
/// Matches CLI plan output colors: create=green, update=yellow, replace=magenta,
/// delete=red, read=cyan.
pub(super) fn build_plan_title(summary: &PlanSummary) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" Plan (Plan: ")];

    for (idx, part) in summary.parts().into_iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw(", "));
        }

        match part {
            PlanSummaryPart::Read { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Cyan),
                ));
                spans.push(Span::raw(" to read"));
            }
            PlanSummaryPart::Import { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Cyan),
                ));
                spans.push(Span::raw(" to import"));
            }
            PlanSummaryPart::Create { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Green),
                ));
                spans.push(Span::raw(" to create"));
            }
            PlanSummaryPart::Update { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
                spans.push(Span::raw(" to update"));
            }
            PlanSummaryPart::Replace { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Magenta),
                ));
                spans.push(Span::raw(" to replace"));
            }
            PlanSummaryPart::Delete { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Red),
                ));
                spans.push(Span::raw(" to delete"));
            }
            PlanSummaryPart::Remove { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
                spans.push(Span::raw(" to remove from state"));
            }
            PlanSummaryPart::Move { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
                spans.push(Span::raw(" to move"));
            }
            PlanSummaryPart::Wait { count } => {
                spans.push(Span::styled(
                    count.to_string(),
                    Style::default().fg(Color::Magenta),
                ));
                spans.push(Span::raw(" to wait"));
            }
        }
    }

    for entry in &summary.deferred {
        spans.push(Span::raw("; "));
        spans.push(Span::styled("N", Style::default().fg(Color::Green)));
        spans.push(Span::raw(" to "));
        match entry.action {
            DeferredSummaryAction::Add => {
                spans.push(Span::styled("add", Style::default().fg(Color::Green)));
            }
            DeferredSummaryAction::Replace => {
                spans.push(Span::styled("replace", Style::default().fg(Color::Magenta)));
            }
        }
        spans.push(Span::raw(format!(
            " after {} applies.",
            entry.upstream_binding
        )));
    }

    spans.push(Span::raw(") "));

    Line::from(spans)
}

/// Return the style for a given effect kind
pub(super) fn effect_style(kind: EffectKind) -> Style {
    match kind {
        EffectKind::Create => Style::default().fg(Color::Green),
        EffectKind::Update => Style::default().fg(Color::Yellow),
        EffectKind::Replace => Style::default().fg(Color::Magenta),
        EffectKind::Delete => Style::default().fg(Color::Red),
        EffectKind::Read => Style::default().fg(Color::Cyan),
        EffectKind::Wait => Style::default().fg(Color::Magenta),
    }
}
