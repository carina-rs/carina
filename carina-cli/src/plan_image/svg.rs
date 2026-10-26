use std::fmt::Write;

use super::layout::{
    Layout, MAX_ATTRIBUTE_NAMES, MAX_LABEL_CHARS, MAX_TYPE_CHARS, MONOSPACE_ADVANCE_EM, NodeRect,
    edge_segment_controls,
};
use super::model::{BlastKind, EffectKind, GraphModel, GraphNode, GraphNodeId};
use crate::plan_summary::PlanEffectSummaryKind;

pub(crate) const FONT_FAMILY: &str =
    "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
const TITLE: &str = concat!("carina", " plan");
const LEGEND_EDGE_SAMPLE_WIDTH: f32 = 44.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SvgDocument {
    pub text: String,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn render_svg(model: &GraphModel, layout: &Layout) -> SvgDocument {
    let mut out = String::new();
    writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" viewBox="0 0 {} {}" role="img" aria-labelledby="title desc">"#,
        layout.width, layout.height, layout.width, layout.height
    )
    .unwrap();
    writeln!(
        out,
        r##"  <rect id="background" fill="#ffffff" x="0" y="0" width="{}" height="{}"/>"##,
        layout.width, layout.height
    )
    .unwrap();
    writeln!(out, "  <title id=\"title\">{TITLE}</title>").unwrap();
    writeln!(
        out,
        "  <desc id=\"desc\">Apply-order dependency graph with destructive-change blast radii.</desc>"
    )
    .unwrap();
    write_markers(&mut out);
    writeln!(
        out,
        "  <g font-family=\"{}\" text-rendering=\"geometricPrecision\">",
        FONT_FAMILY
    )
    .unwrap();

    render_header(&mut out, model);
    render_module_bands(&mut out, model, layout);
    render_edges(&mut out, model, layout);
    render_nodes(&mut out, model, layout);
    render_legend(&mut out, model, layout);

    writeln!(out, "  </g>").unwrap();
    writeln!(out, "</svg>").unwrap();
    SvgDocument {
        text: out,
        width: layout.width,
        height: layout.height,
    }
}

#[derive(Clone, Copy)]
struct KindStyle {
    symbol: &'static str,
    label: &'static str,
    color: &'static str,
}

fn kind_style(kind: EffectKind) -> KindStyle {
    match kind {
        EffectKind::Read => KindStyle {
            symbol: "r",
            label: "read",
            color: "#0969da",
        },
        EffectKind::Create => KindStyle {
            symbol: "+",
            label: "create",
            color: "#1a7f37",
        },
        EffectKind::ResumeCreate => KindStyle {
            symbol: "↻",
            label: "resume",
            color: "#1a7f37",
        },
        EffectKind::Update => KindStyle {
            symbol: "~",
            label: "update",
            color: "#9a6700",
        },
        EffectKind::Replace => KindStyle {
            symbol: "±",
            label: "replace",
            color: "#bc4c00",
        },
        EffectKind::Delete => KindStyle {
            symbol: "-",
            label: "delete",
            color: "#cf222e",
        },
        EffectKind::Import => KindStyle {
            symbol: "i",
            label: "import",
            color: "#0969da",
        },
        EffectKind::Remove => KindStyle {
            symbol: "x",
            label: "remove",
            color: "#57606a",
        },
        EffectKind::Move => KindStyle {
            symbol: "→",
            label: "move",
            color: "#0969da",
        },
        EffectKind::Wait => KindStyle {
            symbol: "w",
            label: "wait",
            color: "#57606a",
        },
        EffectKind::DeferredCreate => KindStyle {
            symbol: "d+",
            label: "deferred create",
            color: "#57606a",
        },
        EffectKind::DeferredReplace => KindStyle {
            symbol: "d±",
            label: "deferred replace",
            color: "#57606a",
        },
        EffectKind::Unchanged => KindStyle {
            symbol: "=",
            label: "unchanged",
            color: "#6e7781",
        },
    }
}

fn summary_label(kind: EffectKind) -> &'static str {
    match kind {
        EffectKind::Read => "to read",
        EffectKind::Create => "to create",
        EffectKind::ResumeCreate => "to resume",
        EffectKind::Update => "to update",
        EffectKind::Replace => "to replace",
        EffectKind::Delete => "to destroy",
        EffectKind::Import => "to import",
        EffectKind::Remove => "to remove",
        EffectKind::Move => "to move",
        EffectKind::Wait => "to wait",
        EffectKind::DeferredCreate => "deferred creates",
        EffectKind::DeferredReplace => "deferred replaces",
        EffectKind::Unchanged => "unchanged",
    }
}

fn render_header(out: &mut String, model: &GraphModel) {
    writeln!(
        out,
        r##"    <text x="28" y="36" font-size="22" font-weight="700" fill="#1f2328">{TITLE}</text>"##
    )
    .unwrap();

    write!(
        out,
        r##"    <text x="28" y="65" font-size="12.5" fill="#57606a">"##
    )
    .unwrap();
    if !model.summary.has_changes() {
        write!(out, r##"<tspan fill="#57606a">No changes</tspan>"##).unwrap();
    } else {
        let mut has_part = false;
        for part in model.summary.effect_parts() {
            write_summary_separator(out, &mut has_part);
            let kind = effect_kind_for_summary(part.kind);
            let style = kind_style(kind);
            let destructive = matches!(kind, EffectKind::Replace | EffectKind::Delete);
            let color = if destructive { "#cf222e" } else { style.color };
            let weight = if destructive { "700" } else { "600" };
            write!(
                out,
                "<tspan fill=\"{color}\" font-weight=\"{weight}\">{} {}</tspan>",
                part.count,
                summary_label(kind)
            )
            .unwrap();
        }
        let export_count = model.summary.export_change_count();
        if export_count > 0 {
            write_summary_separator(out, &mut has_part);
            let label = if export_count == 1 {
                "export to change"
            } else {
                "exports to change"
            };
            write!(
                out,
                r##"<tspan fill="#0969da" font-weight="600">{export_count} {label}</tspan>"##
            )
            .unwrap();
        }
        let deferred_count = model.summary.residual_deferred_for_count();
        if deferred_count > 0 {
            write_summary_separator(out, &mut has_part);
            let label = if deferred_count == 1 {
                "deferred loop pending"
            } else {
                "deferred loops pending"
            };
            write!(
                out,
                r##"<tspan fill="#57606a" font-weight="600">{deferred_count} {label}</tspan>"##
            )
            .unwrap();
        }
    }
    writeln!(out, "</text>").unwrap();

    if let Some(state) = &model.state {
        writeln!(
            out,
            r##"    <text x="28" y="90" font-size="10.5" fill="#6e7781">state {} · serial {}</text>"##,
            escape(&state.lineage),
            state.serial
        )
        .unwrap();
    }
}

fn write_summary_separator(out: &mut String, has_part: &mut bool) {
    if *has_part {
        write!(out, r##"<tspan fill="#8c959f"> · </tspan>"##).unwrap();
    }
    *has_part = true;
}

fn effect_kind_for_summary(kind: PlanEffectSummaryKind) -> EffectKind {
    match kind {
        PlanEffectSummaryKind::Read => EffectKind::Read,
        PlanEffectSummaryKind::Create => EffectKind::Create,
        PlanEffectSummaryKind::ResumeCreate => EffectKind::ResumeCreate,
        PlanEffectSummaryKind::Update => EffectKind::Update,
        PlanEffectSummaryKind::Replace => EffectKind::Replace,
        PlanEffectSummaryKind::Delete => EffectKind::Delete,
        PlanEffectSummaryKind::Import => EffectKind::Import,
        PlanEffectSummaryKind::Remove => EffectKind::Remove,
        PlanEffectSummaryKind::Move => EffectKind::Move,
        PlanEffectSummaryKind::Wait => EffectKind::Wait,
        PlanEffectSummaryKind::DeferredCreate => EffectKind::DeferredCreate,
        PlanEffectSummaryKind::DeferredReplace => EffectKind::DeferredReplace,
    }
}

fn write_markers(out: &mut String) {
    writeln!(out, "  <defs>").unwrap();
    for (id, color) in [
        ("default", "#8c959f"),
        ("replace", "#bc4c00"),
        ("delete", "#cf222e"),
    ] {
        writeln!(
            out,
            "    <marker id=\"arrow-{id}\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"4\" orient=\"auto\" markerUnits=\"strokeWidth\"><path d=\"M0,0 L8,4 L0,8 z\" fill=\"{color}\"/></marker>"
        )
        .unwrap();
    }
    writeln!(out, "  </defs>").unwrap();
}

fn render_module_bands(out: &mut String, model: &GraphModel, layout: &Layout) {
    for rect in &layout.modules {
        let Some(module) = model.modules.iter().find(|module| module.id == rect.id) else {
            continue;
        };
        writeln!(
            out,
            r##"    <rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="12" fill="#f6f8fa" stroke="#d0d7de"/>"##,
            rect.x, rect.y, rect.width, rect.height
        )
        .unwrap();
        let source = module
            .source_path
            .as_deref()
            .map(|path| format!(" ({path})"))
            .unwrap_or_default();
        let collapsed = if module.collapsed {
            " · collapsed"
        } else {
            ""
        };
        writeln!(
            out,
            r##"    <text x="{:.1}" y="{:.1}" font-size="11" font-weight="600" fill="#57606a">module &quot;{}&quot;{}{}</text>"##,
            rect.x + 14.0,
            rect.y + 20.0,
            escape(&module.label),
            escape(&source),
            collapsed
        )
        .unwrap();
    }
}

fn render_edges(out: &mut String, model: &GraphModel, layout: &Layout) {
    let mut edges: Vec<_> = model
        .ordering_edges
        .iter()
        .map(|edge| (edge, false))
        .chain(model.reference_edges.iter().map(|edge| (edge, true)))
        .collect();
    edges.sort_by_key(|(edge, is_reference)| {
        let node_key = |id: &GraphNodeId| {
            model.node(id).map_or_else(
                || (String::new(), String::new(), id.stable_key()),
                |node| {
                    (
                        node.resource_type.clone(),
                        node.label.clone(),
                        id.stable_key(),
                    )
                },
            )
        };
        (*is_reference, node_key(&edge.from), node_key(&edge.to))
    });
    for (edge, is_reference) in edges {
        if layout.node(&edge.from).is_none() || layout.node(&edge.to).is_none() {
            continue;
        }
        let Some(route) = layout.edge_route(&edge.from, &edge.to) else {
            continue;
        };
        let Some(first) = route.points.first() else {
            continue;
        };
        let mut path = format!("M{:.1},{:.1}", first.x, first.y);
        for segment in route.points.windows(2) {
            let (control_one, control_two) = edge_segment_controls(segment[0], segment[1]);
            write!(
                path,
                " C{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}",
                control_one.x,
                control_one.y,
                control_two.x,
                control_two.y,
                segment[1].x,
                segment[1].y
            )
            .unwrap();
        }
        let (color, marker, opacity, width) = match edge.highlight {
            Some(BlastKind::Replace) => ("#bc4c00", "replace", 0.76, 2.2),
            Some(BlastKind::Delete) => ("#cf222e", "delete", 0.76, 2.2),
            None => ("#8c959f", "default", 0.68, 1.6),
        };
        let dash = if is_reference {
            " stroke-dasharray=\"6 5\""
        } else {
            ""
        };
        writeln!(
            out,
            "    <path d=\"{path}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"{width:.1}\" stroke-opacity=\"{opacity:.2}\"{dash} marker-end=\"url(#arrow-{marker})\"/>",
        )
        .unwrap();
    }
}

fn render_nodes(out: &mut String, model: &GraphModel, layout: &Layout) {
    for rect in &layout.nodes {
        if let Some(node) = model.node(&rect.id) {
            render_node(out, node, rect);
        }
    }
}

fn render_node(out: &mut String, node: &GraphNode, rect: &NodeRect) {
    let style = kind_style(node.kind);
    if let Some(halo) = node_halo(node) {
        let color = match halo {
            BlastKind::Replace => "#bc4c00",
            BlastKind::Delete => "#cf222e",
        };
        writeln!(
            out,
            "    <rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"14\" fill=\"none\" stroke=\"{color}\" stroke-width=\"8\" stroke-opacity=\"0.24\"/>",
            rect.x - 5.0,
            rect.y - 5.0,
            rect.width + 10.0,
            rect.height + 10.0,
        )
        .unwrap();
    }

    let (fill, border, fill_opacity) = if node.kind == EffectKind::Unchanged {
        ("#f6f8fa", "#afb8c1", "0.72")
    } else {
        ("#ffffff", style.color, "1")
    };
    let dash = if node.kind == EffectKind::Delete {
        " stroke-dasharray=\"6 4\""
    } else {
        ""
    };
    writeln!(out, "    <g>").unwrap();
    writeln!(
        out,
        "      <rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"9\" fill=\"{fill}\" fill-opacity=\"{fill_opacity}\" stroke=\"{border}\"{dash}/>",
        rect.x, rect.y, rect.width, rect.height
    )
    .unwrap();
    if node.kind != EffectKind::Unchanged {
        writeln!(
            out,
            "      <rect x=\"{:.1}\" y=\"{:.1}\" width=\"6\" height=\"{:.1}\" rx=\"3\" fill=\"{}\"/>",
            rect.x,
            rect.y,
            rect.height,
            style.color
        )
        .unwrap();
    }

    let badge_x = if node.kind == EffectKind::Unchanged {
        rect.x + 14.0
    } else {
        writeln!(
            out,
            "      <text x=\"{:.1}\" y=\"{:.1}\" font-size=\"13\" font-weight=\"700\" fill=\"{}\">{}</text>",
            rect.x + 17.0,
            rect.y + 27.0,
            style.color,
            escape(style.symbol)
        )
        .unwrap();
        rect.x + 38.0
    };
    let badge_width = text_width(style.label, 10.0) + 16.0;
    writeln!(
        out,
        "      <rect x=\"{badge_x:.1}\" y=\"{:.1}\" width=\"{badge_width:.1}\" height=\"20\" rx=\"10\" fill=\"{}\" fill-opacity=\"0.10\"/>",
        rect.y + 10.0,
        style.color
    )
    .unwrap();
    writeln!(
        out,
        "      <text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" font-weight=\"700\" fill=\"{}\">{}</text>",
        badge_x + 8.0,
        rect.y + 24.5,
        style.color,
        escape(style.label)
    )
    .unwrap();

    let text_x = rect.x + 18.0;
    let label_y = if node.kind == EffectKind::Unchanged {
        rect.y + 45.0
    } else {
        rect.y + 49.0
    };
    let decoration = if node.kind == EffectKind::Delete {
        " text-decoration=\"line-through\""
    } else {
        ""
    };
    writeln!(
        out,
        "      <text x=\"{text_x:.1}\" y=\"{label_y:.1}\" font-size=\"14\" font-weight=\"700\" fill=\"#1f2328\"{decoration}>{}</text>",
        escape(&truncate(&node.label, MAX_LABEL_CHARS))
    )
    .unwrap();
    let type_y = if node.kind == EffectKind::Unchanged {
        rect.y + 61.0
    } else {
        rect.y + 67.0
    };
    writeln!(
        out,
        r##"      <text x="{text_x:.1}" y="{type_y:.1}" font-size="11" fill="#6e7781">{}</text>"##,
        escape(&truncate(&node.resource_type, MAX_TYPE_CHARS))
    )
    .unwrap();

    let mut cursor = rect.y + 85.0;
    if let Some(note) = &node.note {
        writeln!(
            out,
            r##"      <text x="{text_x:.1}" y="{cursor:.1}" font-size="10.5" fill="#57606a">{}</text>"##,
            escape(&truncate(note, MAX_TYPE_CHARS))
        )
        .unwrap();
        cursor += 17.0;
    }
    if matches!(node.kind, EffectKind::Update | EffectKind::Replace)
        && !node.attribute_names.is_empty()
    {
        let heading = if node.kind == EffectKind::Replace {
            "forces replacement:"
        } else {
            "changed attributes:"
        };
        writeln!(
            out,
            "      <text x=\"{text_x:.1}\" y=\"{cursor:.1}\" font-size=\"10.5\" font-weight=\"600\" fill=\"{}\">{heading}</text>",
            style.color
        )
        .unwrap();
        cursor += 16.0;
        for attribute in node.attribute_names.iter().take(MAX_ATTRIBUTE_NAMES) {
            writeln!(
                out,
                r##"      <text x="{:.1}" y="{cursor:.1}" font-size="10.5" fill="#24292f">• {}</text>"##,
                text_x + 8.0,
                escape(&truncate(attribute, MAX_TYPE_CHARS))
            )
            .unwrap();
            cursor += 16.0;
        }
        if node.attribute_names.len() > MAX_ATTRIBUTE_NAMES {
            writeln!(
                out,
                r##"      <text x="{:.1}" y="{cursor:.1}" font-size="10.5" fill="#6e7781">+{} more</text>"##,
                text_x + 8.0,
                node.attribute_names.len() - MAX_ATTRIBUTE_NAMES
            )
            .unwrap();
        }
    }

    if !node.affected_by.is_empty() {
        let halo = node_halo(node).expect("affected node has a halo kind");
        let color = match halo {
            BlastKind::Replace => "#bc4c00",
            BlastKind::Delete => "#cf222e",
        };
        let affected = if node.affected_by.len() == 1 {
            format!("affected by {}", node.affected_by[0].label)
        } else {
            format!("affected by {} changes", node.affected_by.len())
        };
        writeln!(
            out,
            "      <text x=\"{text_x:.1}\" y=\"{:.1}\" font-size=\"10\" font-weight=\"600\" fill=\"{color}\">{}</text>",
            rect.y + rect.height - 10.0,
            escape(&truncate(&affected, MAX_TYPE_CHARS))
        )
        .unwrap();
    }
    writeln!(out, "    </g>").unwrap();
}

fn node_halo(node: &GraphNode) -> Option<BlastKind> {
    if node
        .affected_by
        .iter()
        .any(|source| source.kind == BlastKind::Delete)
    {
        Some(BlastKind::Delete)
    } else if node
        .affected_by
        .iter()
        .any(|source| source.kind == BlastKind::Replace)
    {
        Some(BlastKind::Replace)
    } else {
        None
    }
}

fn render_legend(out: &mut String, model: &GraphModel, layout: &Layout) {
    let kinds = [
        EffectKind::Read,
        EffectKind::Create,
        EffectKind::ResumeCreate,
        EffectKind::Update,
        EffectKind::Replace,
        EffectKind::Delete,
        EffectKind::Import,
        EffectKind::Remove,
        EffectKind::Move,
        EffectKind::Wait,
        EffectKind::DeferredCreate,
        EffectKind::DeferredReplace,
        EffectKind::Unchanged,
    ];
    let visible_kinds: Vec<_> = kinds
        .into_iter()
        .filter(|kind| model.nodes.iter().any(|node| node.kind == *kind))
        .collect();
    let has_halo = model.nodes.iter().any(|node| !node.affected_by.is_empty());
    let has_ordering_edges = !model.ordering_edges.is_empty();
    let has_reference_edges = !model.reference_edges.is_empty();
    if visible_kinds.is_empty() && !has_halo && !has_ordering_edges && !has_reference_edges {
        return;
    }

    let divider_y = layout.height as f32 - 82.0;
    writeln!(
        out,
        r##"    <line x1="28" y1="{divider_y:.1}" x2="{}" y2="{divider_y:.1}" stroke="#d8dee4"/>"##,
        layout.width - 28
    )
    .unwrap();
    let mut x = 28.0;
    let mut y = divider_y + 21.0;
    for kind in visible_kinds {
        let style = kind_style(kind);
        let item_width = text_width(style.symbol, 11.0) + text_width(style.label, 10.0) + 28.0;
        if x + item_width > layout.width as f32 - 28.0 {
            x = 28.0;
            y += 20.0;
        }
        writeln!(
            out,
            "    <text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"11\" font-weight=\"700\" fill=\"{}\">{}</text>",
            style.color,
            escape(style.symbol)
        )
        .unwrap();
        let label_x = x + text_width(style.symbol, 11.0) + 6.0;
        writeln!(
            out,
            r##"    <text x="{label_x:.1}" y="{y:.1}" font-size="10" fill="#57606a">{}</text>"##,
            escape(style.label)
        )
        .unwrap();
        x += item_width;
    }
    let semantics_y = layout.height as f32 - 10.0;
    let mut semantics_x = 28.0;
    if has_ordering_edges {
        writeln!(
            out,
            "    <line x1=\"{semantics_x:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#8c959f\" stroke-width=\"1.6\" marker-end=\"url(#arrow-default)\"/><text x=\"{:.1}\" y=\"{semantics_y:.1}\" font-size=\"10\" fill=\"#57606a\">runs before</text>",
            semantics_y - 4.0,
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH,
            semantics_y - 4.0,
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH + 10.0,
        )
        .unwrap();
        semantics_x += 146.0;
    }
    if has_reference_edges {
        writeln!(
            out,
            "    <line x1=\"{semantics_x:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#8c959f\" stroke-width=\"1.6\" stroke-dasharray=\"6 5\" marker-end=\"url(#arrow-default)\"/><text x=\"{:.1}\" y=\"{semantics_y:.1}\" font-size=\"10\" fill=\"#57606a\">references (unchanged)</text>",
            semantics_y - 4.0,
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH,
            semantics_y - 4.0,
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH + 10.0,
        )
        .unwrap();
        semantics_x += 208.0;
    }
    if has_halo {
        writeln!(
            out,
            "    <rect x=\"{semantics_x:.1}\" y=\"{:.1}\" width=\"24\" height=\"13\" rx=\"4\" fill=\"none\" stroke=\"#bc4c00\" stroke-width=\"5\" stroke-opacity=\"0.24\"/><text x=\"{:.1}\" y=\"{semantics_y:.1}\" font-size=\"10\" fill=\"#57606a\">halo = downstream blast radius</text>",
            semantics_y - 13.0,
            semantics_x + 34.0,
        )
        .unwrap();
    }
}

fn text_width(text: &str, font_size: f32) -> f32 {
    text.chars().count() as f32 * font_size * MONOSPACE_ADVANCE_EM
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars()
        .take(max_chars.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\t' | '\n' | '\r' => escaped.push(character),
            control if control < '\u{20}' => escaped.push('\u{fffd}'),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use carina_core::effect::Effect;
    use carina_core::executor::UnresolvedResource;
    use carina_core::plan::Plan;
    use carina_core::provider::AppliedIdentifier;
    use carina_core::resource::{
        ConcreteValue, DeferredValue, ResolvedResource, Resource, State, Value,
    };

    use super::*;
    use crate::plan_image::layout::layout_graph;
    use crate::plan_image::model::{PlanImageInput, StateMetadata, build_model};

    fn fixture_svg(name: &str) -> String {
        let fixture = crate::fixture_plan::build_plan_from_fixture_name(name);
        let state = fixture.state_file.as_ref().map(|state| StateMetadata {
            lineage: &state.lineage,
            serial: state.serial,
        });
        let summary = crate::fixture_plan::build_fixture_plan_change_summary(&fixture);
        let model = build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &fixture.desired_resources,
            unresolved_resources: &fixture.unresolved_resources,
            compositions: &fixture.compositions,
            expansion_trace: Some(&fixture.expansion_trace),
            state,
            summary: &summary,
        });
        let layout = layout_graph(&model);
        render_svg(&model, &layout).text
    }

    #[test]
    fn exports_only_summary_is_not_no_changes() {
        let fixture =
            crate::fixture_plan::build_plan_from_fixture_name("exports_multifile_let_chain");
        assert!(fixture.plan.effects().is_empty());
        let svg = fixture_svg("exports_multifile_let_chain");

        assert!(svg.contains("1 export to change"));
        assert!(!svg.contains("No changes"));
    }

    #[test]
    fn residual_deferred_only_summary_is_not_no_changes() {
        let fixture = crate::fixture_plan::build_plan_from_fixture_name("deferred_for");
        assert!(fixture.plan.effects().is_empty());
        let svg = fixture_svg("deferred_for");

        assert!(svg.contains("1 deferred loop pending"));
        assert!(!svg.contains("No changes"));
    }

    #[test]
    fn svg_never_contains_secret_or_write_only_attribute_values() {
        let mut resource = Resource::new("test.Credential", "credential");
        resource.binding = Some("credential".to_string());
        resource.set_attr(
            "secret_token",
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("SECRET-SENTINEL-3966".into()),
            )))),
        );
        resource.set_attr(
            "write_only_token",
            Value::Concrete(ConcreteValue::String("WRITE-ONLY-SENTINEL-3966".into())),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(State::existing(resource.id.clone(), HashMap::new())),
            identifier: AppliedIdentifier::new("credential-id").unwrap(),
            to: ResolvedResource::new(resource.clone()),
            changed_attributes: vec!["secret_token".to_string(), "write_only_token".to_string()],
        });
        let desired = vec![ResolvedResource::new(resource)];
        let summary = crate::plan_summary::PlanChangeSummary::new(&plan, &[], &[]);
        let model = build_model(PlanImageInput {
            plan: &plan,
            desired_resources: &desired,
            unresolved_resources: &[],
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary: &summary,
        });
        let svg = render_svg(&model, &layout_graph(&model)).text;

        assert!(svg.contains("secret_token"));
        assert!(svg.contains("write_only_token"));
        assert!(!svg.contains("SECRET-SENTINEL-3966"));
        assert!(!svg.contains("WRITE-ONLY-SENTINEL-3966"));
    }

    #[test]
    fn svg_truncates_long_labels_and_caps_attribute_names() {
        let long_label = "this_label_is_intentionally_far_too_long_for_the_card";
        let mut resource = Resource::new("test.Widget", "long-label-resource");
        resource.binding = Some(long_label.to_string());
        let changed_attributes: Vec<_> = (0..8)
            .map(|index| format!("attribute_{index:02}"))
            .collect();
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(State::existing(resource.id.clone(), HashMap::new())),
            identifier: AppliedIdentifier::new("long-label-id").unwrap(),
            to: ResolvedResource::new(resource.clone()),
            changed_attributes,
        });
        let desired = vec![ResolvedResource::new(resource)];
        let summary = crate::plan_summary::PlanChangeSummary::new(&plan, &[], &[]);
        let model = build_model(PlanImageInput {
            plan: &plan,
            desired_resources: &desired,
            unresolved_resources: &[],
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary: &summary,
        });
        let svg = render_svg(&model, &layout_graph(&model)).text;

        assert!(!svg.contains(long_label));
        assert!(svg.contains('…'));
        assert!(svg.contains("• attribute_05"));
        assert!(!svg.contains("• attribute_06"));
        assert!(svg.contains("+2 more"));
    }

    #[test]
    fn svg_has_explicit_white_background_and_vendored_font_family() {
        let svg = fixture_svg("mixed_operations");
        assert!(svg.contains(r##"id="background" fill="#ffffff""##));
        assert!(svg.contains(FONT_FAMILY));
    }

    #[test]
    fn legend_contains_only_visible_kinds_and_applicable_semantics() {
        let mixed = fixture_svg("mixed_operations");
        assert!(mixed.contains(">create</text>"));
        assert!(mixed.contains(">update</text>"));
        assert!(mixed.contains(">delete</text>"));
        assert!(mixed.contains(">runs before</text>"));
        assert!(!mixed.contains(">read</text>"));
        assert!(!mixed.contains(">replace</text>"));
        assert!(!mixed.contains(">unchanged</text>"));
        assert!(!mixed.contains("halo = downstream blast radius"));

        let delete = fixture_svg("delete_orphan");
        assert!(!delete.contains(">runs before</text>"));

        let cascade = fixture_svg("plan_image_module_cascade");
        assert!(cascade.contains(">unchanged</text>"));
        assert!(cascade.contains(">references (unchanged)</text>"));
        assert!(cascade.contains("halo = downstream blast radius"));
    }

    #[test]
    fn reference_edges_are_dashed_and_do_not_claim_execution_order() {
        let mut a = Resource::new("test.Widget", "a");
        a.binding = Some("a".to_string());
        let mut connector = Resource::new("test.Widget", "connector");
        connector.binding = Some("connector".to_string());
        connector.set_attr(
            "input",
            Value::resource_ref("a".to_string(), "id".to_string(), vec![]),
        );
        let mut b = Resource::new("test.Widget", "b");
        b.binding = Some("b".to_string());
        b.set_attr(
            "input",
            Value::resource_ref("connector".to_string(), "id".to_string(), vec![]),
        );
        let mut plan = Plan::new();
        for resource in [a.clone(), b.clone()] {
            plan.add(Effect::Update {
                from: Box::new(State::existing(resource.id.clone(), HashMap::new())),
                identifier: AppliedIdentifier::new(format!(
                    "{}-id",
                    resource.binding.as_deref().expect("test resource is bound")
                ))
                .unwrap(),
                to: ResolvedResource::new(resource),
                changed_attributes: vec!["description".to_string()],
            });
        }
        let desired = ResolvedResource::collect_resolved([a, connector, b]).unwrap();
        let unresolved: Vec<_> = desired
            .iter()
            .map(|resource| UnresolvedResource::from_pre_resolve(resource.as_inner().clone()))
            .collect();
        let summary = crate::plan_summary::PlanChangeSummary::new(&plan, &[], &[]);
        let model = build_model(PlanImageInput {
            plan: &plan,
            desired_resources: &desired,
            unresolved_resources: &unresolved,
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary: &summary,
        });
        let svg = render_svg(&model, &layout_graph(&model)).text;

        assert!(svg.contains("stroke-dasharray=\"6 5\""));
        assert!(svg.contains(">references (unchanged)</text>"));
        assert!(!svg.contains(">runs before</text>"));
        let sample = svg
            .lines()
            .find(|line| {
                line.contains("stroke-dasharray=\"6 5\"")
                    && line.contains(">references (unchanged)</text>")
            })
            .expect("dashed legend sample");
        assert!(sample.contains("x1=\"28.0\""));
        assert!(
            sample.contains("x2=\"72.0\""),
            "legend sample must be long enough to show multiple dashes: {sample}"
        );
    }

    #[test]
    fn escape_replaces_xml_illegal_control_characters() {
        assert_eq!(
            escape("a\u{0}b\u{8}\t\n\r\u{b}\u{c}\u{1f}z"),
            "a\u{fffd}b\u{fffd}\t\n\r\u{fffd}\u{fffd}\u{fffd}z"
        );
    }

    #[test]
    fn svg_is_identical_when_the_fixture_plan_is_built_twice() {
        let left = fixture_svg("plan_image_module_cascade");
        let right = fixture_svg("plan_image_module_cascade");
        if left != right {
            for (index, (left_line, right_line)) in left.lines().zip(right.lines()).enumerate() {
                if left_line != right_line {
                    eprintln!("first SVG difference at line {}", index + 1);
                    eprintln!("left:  {left_line}");
                    eprintln!("right: {right_line}");
                    break;
                }
            }
        }
        assert_eq!(left, right);
    }

    #[test]
    fn snapshot_mixed_operations_svg() {
        insta::assert_snapshot!(fixture_svg("mixed_operations"));
    }

    #[test]
    fn snapshot_replace_create_only_svg() {
        insta::assert_snapshot!(fixture_svg("replace_create_only"));
    }

    #[test]
    fn snapshot_delete_svg() {
        insta::assert_snapshot!(fixture_svg("delete_orphan"));
    }

    #[test]
    fn snapshot_module_svg() {
        insta::assert_snapshot!(fixture_svg("module_anonymous_resource"));
    }

    #[test]
    fn snapshot_module_cascade_svg() {
        insta::assert_snapshot!(fixture_svg("plan_image_module_cascade"));
    }
}
