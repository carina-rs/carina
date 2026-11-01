use std::fmt::Write;

use super::layout::{
    Layout, MAX_ATTRIBUTE_NAMES, MAX_LABEL_CHARS, MAX_TYPE_CHARS, MONOSPACE_ADVANCE_EM,
    NODE_HALO_OFFSET, NODE_HALO_STROKE_WIDTH, NodeRect, edge_segment_controls, truncate_middle,
};
use super::model::{BlastKind, EffectKind, GraphModel, GraphNode, GraphNodeId};
use super::{SvgTheme, Theme};
use crate::plan_summary::PlanEffectSummaryKind;

pub(crate) const FONT_FAMILY: &str =
    "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
const TITLE: &str = concat!("carina", " plan");
const LEGEND_EDGE_SAMPLE_WIDTH: f32 = 44.0;

#[derive(Clone, Copy)]
struct EffectPalette {
    color: &'static str,
    tint: &'static str,
}

#[derive(Clone, Copy)]
struct Palette {
    background: &'static str,
    surface: &'static str,
    primary_text: &'static str,
    body_text: &'static str,
    muted_text: &'static str,
    subtle_text: &'static str,
    edge: &'static str,
    border: &'static str,
    divider: &'static str,
    unchanged_border: &'static str,
    teal: EffectPalette,
    create: EffectPalette,
    update: EffectPalette,
    replace: EffectPalette,
    delete: EffectPalette,
    gray: EffectPalette,
    unchanged: EffectPalette,
    halo_opacity: &'static str,
    unchanged_opacity: &'static str,
}

const LIGHT_PALETTE: Palette = Palette {
    background: "#ffffff",
    surface: "#f6f8fa",
    primary_text: "#1f2328",
    body_text: "#24292f",
    muted_text: "#57606a",
    subtle_text: "#6e7781",
    edge: "#8c959f",
    border: "#d0d7de",
    divider: "#d8dee4",
    unchanged_border: "#afb8c1",
    teal: EffectPalette {
        color: "#1b7c83",
        tint: "#d8f3f6",
    },
    create: EffectPalette {
        color: "#1a7f37",
        tint: "#dafbe1",
    },
    update: EffectPalette {
        color: "#0969da",
        tint: "#ddf4ff",
    },
    replace: EffectPalette {
        color: "#8250df",
        tint: "#fbefff",
    },
    delete: EffectPalette {
        color: "#cf222e",
        tint: "#ffebe9",
    },
    gray: EffectPalette {
        color: "#57606a",
        tint: "#f6f8fa",
    },
    unchanged: EffectPalette {
        color: "#6e7781",
        tint: "#f6f8fa",
    },
    halo_opacity: "0.24",
    unchanged_opacity: "0.72",
};

const DARK_PALETTE: Palette = Palette {
    background: "#0d1117",
    surface: "#161b22",
    primary_text: "#e6edf3",
    body_text: "#e6edf3",
    muted_text: "#8b949e",
    subtle_text: "#8b949e",
    edge: "#6e7681",
    border: "#30363d",
    divider: "#30363d",
    unchanged_border: "#30363d",
    teal: EffectPalette {
        color: "#39c5cf",
        tint: "#0f2629",
    },
    create: EffectPalette {
        color: "#3fb950",
        tint: "#12261e",
    },
    update: EffectPalette {
        color: "#4493f8",
        tint: "#121d2f",
    },
    replace: EffectPalette {
        color: "#ab7df8",
        tint: "#1f1530",
    },
    delete: EffectPalette {
        color: "#f85149",
        tint: "#2d1517",
    },
    gray: EffectPalette {
        color: "#8b949e",
        tint: "#161b22",
    },
    unchanged: EffectPalette {
        color: "#8b949e",
        tint: "#161b22",
    },
    halo_opacity: "0.38",
    unchanged_opacity: "1",
};

#[derive(Clone, Copy)]
struct RenderStyle {
    palette: &'static Palette,
    auto: bool,
}

impl RenderStyle {
    fn new(theme: SvgTheme) -> Self {
        match theme {
            SvgTheme::Fixed(Theme::Light) => Self {
                palette: &LIGHT_PALETTE,
                auto: false,
            },
            SvgTheme::Fixed(Theme::Dark) => Self {
                palette: &DARK_PALETTE,
                auto: false,
            },
            SvgTheme::Auto => Self {
                palette: &LIGHT_PALETTE,
                auto: true,
            },
        }
    }

    fn class(self, names: &str) -> String {
        if self.auto {
            format!(" class=\"{names}\"")
        } else {
            String::new()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SvgDocument {
    pub text: String,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn render_svg(model: &GraphModel, layout: &Layout, theme: SvgTheme) -> SvgDocument {
    let style = RenderStyle::new(theme);
    let mut out = String::new();
    writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" viewBox="0 0 {} {}" role="img" aria-labelledby="title desc">"#,
        layout.width, layout.height, layout.width, layout.height
    )
    .unwrap();
    writeln!(
        out,
        r#"  <rect id="background"{} fill="{}" x="0" y="0" width="{}" height="{}"/>"#,
        style.class("fill-background"),
        style.palette.background,
        layout.width,
        layout.height
    )
    .unwrap();
    writeln!(out, "  <title id=\"title\">{TITLE}</title>").unwrap();
    writeln!(
        out,
        "  <desc id=\"desc\">Apply-order dependency graph with destructive-change blast radii.</desc>"
    )
    .unwrap();
    if style.auto {
        write_auto_palette_css(&mut out);
    }
    write_markers(&mut out, style);
    writeln!(
        out,
        "  <g font-family=\"{}\" text-rendering=\"geometricPrecision\">",
        FONT_FAMILY
    )
    .unwrap();

    render_header(&mut out, model, style);
    render_module_band_backgrounds(&mut out, layout, style);
    render_edges(&mut out, model, layout, style);
    render_module_band_labels(&mut out, model, layout, style);
    render_nodes(&mut out, model, layout, style);
    render_legend(&mut out, model, layout, style);

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
    tint: &'static str,
    fill_class: &'static str,
    stroke_class: &'static str,
    tint_class: &'static str,
}

fn kind_style(kind: EffectKind, palette: &Palette) -> KindStyle {
    match kind {
        EffectKind::Read => KindStyle {
            symbol: "r",
            label: "read",
            color: palette.teal.color,
            tint: palette.teal.tint,
            fill_class: "fill-teal",
            stroke_class: "stroke-teal",
            tint_class: "fill-teal-tint",
        },
        EffectKind::Create => KindStyle {
            symbol: "+",
            label: "create",
            color: palette.create.color,
            tint: palette.create.tint,
            fill_class: "fill-create",
            stroke_class: "stroke-create",
            tint_class: "fill-create-tint",
        },
        EffectKind::ResumeCreate => KindStyle {
            symbol: "↻",
            label: "resume",
            color: palette.create.color,
            tint: palette.create.tint,
            fill_class: "fill-create",
            stroke_class: "stroke-create",
            tint_class: "fill-create-tint",
        },
        EffectKind::Update => KindStyle {
            symbol: "~",
            label: "update",
            color: palette.update.color,
            tint: palette.update.tint,
            fill_class: "fill-update",
            stroke_class: "stroke-update",
            tint_class: "fill-update-tint",
        },
        EffectKind::Replace => KindStyle {
            symbol: "±",
            label: "replace",
            color: palette.replace.color,
            tint: palette.replace.tint,
            fill_class: "fill-replace",
            stroke_class: "stroke-replace",
            tint_class: "fill-replace-tint",
        },
        EffectKind::Delete => KindStyle {
            symbol: "-",
            label: "delete",
            color: palette.delete.color,
            tint: palette.delete.tint,
            fill_class: "fill-delete",
            stroke_class: "stroke-delete",
            tint_class: "fill-delete-tint",
        },
        EffectKind::Import => KindStyle {
            symbol: "i",
            label: "import",
            color: palette.teal.color,
            tint: palette.teal.tint,
            fill_class: "fill-teal",
            stroke_class: "stroke-teal",
            tint_class: "fill-teal-tint",
        },
        EffectKind::Remove => KindStyle {
            symbol: "x",
            label: "remove",
            color: palette.gray.color,
            tint: palette.gray.tint,
            fill_class: "fill-gray",
            stroke_class: "stroke-gray",
            tint_class: "fill-gray-tint",
        },
        EffectKind::Move => KindStyle {
            symbol: "→",
            label: "move",
            color: palette.teal.color,
            tint: palette.teal.tint,
            fill_class: "fill-teal",
            stroke_class: "stroke-teal",
            tint_class: "fill-teal-tint",
        },
        EffectKind::Release => KindStyle {
            symbol: "!",
            label: "release",
            color: palette.update.color,
            tint: palette.update.tint,
            fill_class: "fill-update",
            stroke_class: "stroke-update",
            tint_class: "fill-update-tint",
        },
        EffectKind::Wait => KindStyle {
            symbol: "w",
            label: "wait",
            color: palette.gray.color,
            tint: palette.gray.tint,
            fill_class: "fill-gray",
            stroke_class: "stroke-gray",
            tint_class: "fill-gray-tint",
        },
        EffectKind::DeferredCreate => KindStyle {
            symbol: "d+",
            label: "deferred create",
            color: palette.gray.color,
            tint: palette.gray.tint,
            fill_class: "fill-gray",
            stroke_class: "stroke-gray",
            tint_class: "fill-gray-tint",
        },
        EffectKind::DeferredReplace => KindStyle {
            symbol: "d±",
            label: "deferred replace",
            color: palette.gray.color,
            tint: palette.gray.tint,
            fill_class: "fill-gray",
            stroke_class: "stroke-gray",
            tint_class: "fill-gray-tint",
        },
        EffectKind::Unchanged => KindStyle {
            symbol: "=",
            label: "unchanged",
            color: palette.unchanged.color,
            tint: palette.unchanged.tint,
            fill_class: "fill-unchanged",
            stroke_class: "stroke-unchanged-kind",
            tint_class: "fill-unchanged-tint",
        },
    }
}

impl BlastKind {
    fn effect_kind(self) -> EffectKind {
        match self {
            Self::Replace => EffectKind::Replace,
            Self::Delete => EffectKind::Delete,
        }
    }

    fn color(self, palette: &Palette) -> &'static str {
        kind_style(self.effect_kind(), palette).color
    }

    fn fill_class(self, palette: &Palette) -> &'static str {
        kind_style(self.effect_kind(), palette).fill_class
    }

    fn stroke_class(self, palette: &Palette) -> &'static str {
        kind_style(self.effect_kind(), palette).stroke_class
    }

    fn marker_id(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Delete => "delete",
        }
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
        EffectKind::Release => "to release",
        EffectKind::Wait => "to wait",
        EffectKind::DeferredCreate => "deferred creates",
        EffectKind::DeferredReplace => "deferred replaces",
        EffectKind::Unchanged => "unchanged",
    }
}

fn render_header(out: &mut String, model: &GraphModel, render_style: RenderStyle) {
    let palette = render_style.palette;
    writeln!(
        out,
        r#"    <text{} x="28" y="36" font-size="22" font-weight="700" fill="{}">{TITLE}</text>"#,
        render_style.class("fill-primary"),
        palette.primary_text,
    )
    .unwrap();

    write!(
        out,
        r#"    <text{} x="28" y="65" font-size="12.5" fill="{}">"#,
        render_style.class("fill-muted"),
        palette.muted_text,
    )
    .unwrap();
    if !model.summary.has_changes() {
        write!(
            out,
            r#"<tspan{} fill="{}">No changes</tspan>"#,
            render_style.class("fill-muted"),
            palette.muted_text,
        )
        .unwrap();
    } else {
        let mut has_part = false;
        for part in model.summary.effect_parts() {
            write_summary_separator(out, &mut has_part, render_style);
            let kind = effect_kind_for_summary(part.kind);
            let style = kind_style(kind, palette);
            let destructive = matches!(kind, EffectKind::Replace | EffectKind::Delete);
            let weight = if destructive { "700" } else { "600" };
            write!(
                out,
                "<tspan{} fill=\"{}\" font-weight=\"{weight}\">{} {}</tspan>",
                render_style.class(style.fill_class),
                style.color,
                part.count,
                summary_label(kind)
            )
            .unwrap();
        }
        let export_count = model.summary.export_change_count();
        if export_count > 0 {
            write_summary_separator(out, &mut has_part, render_style);
            let label = if export_count == 1 {
                "export to change"
            } else {
                "exports to change"
            };
            write!(
                out,
                r#"<tspan{} fill="{}" font-weight="600">{export_count} {label}</tspan>"#,
                render_style.class("fill-muted"),
                palette.muted_text,
            )
            .unwrap();
        }
        let directive_count = model.summary.persisted_directive_change_count();
        if directive_count > 0 {
            write_summary_separator(out, &mut has_part, render_style);
            let label = if directive_count == 1 {
                "resource directive change"
            } else {
                "resource directive changes"
            };
            write!(
                out,
                r#"<tspan{} fill="{}" font-weight="600">{directive_count} {label}</tspan>"#,
                render_style.class("fill-muted"),
                palette.muted_text,
            )
            .unwrap();
        }
        let deferred_count = model.summary.residual_deferred_for_count();
        if deferred_count > 0 {
            write_summary_separator(out, &mut has_part, render_style);
            let label = if deferred_count == 1 {
                "deferred loop pending"
            } else {
                "deferred loops pending"
            };
            write!(
                out,
                r#"<tspan{} fill="{}" font-weight="600">{deferred_count} {label}</tspan>"#,
                render_style.class("fill-muted"),
                palette.muted_text,
            )
            .unwrap();
        }
    }
    writeln!(out, "</text>").unwrap();

    if let Some(state) = &model.state {
        writeln!(
            out,
            r#"    <text{} x="28" y="90" font-size="10.5" fill="{}">state {} · serial {}</text>"#,
            render_style.class("fill-subtle"),
            palette.subtle_text,
            escape(&state.lineage),
            state.serial
        )
        .unwrap();
    }
}

fn write_summary_separator(out: &mut String, has_part: &mut bool, render_style: RenderStyle) {
    if *has_part {
        write!(
            out,
            r#"<tspan{} fill="{}"> · </tspan>"#,
            render_style.class("fill-edge"),
            render_style.palette.edge,
        )
        .unwrap();
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
        PlanEffectSummaryKind::Release => EffectKind::Release,
        PlanEffectSummaryKind::Wait => EffectKind::Wait,
        PlanEffectSummaryKind::DeferredCreate => EffectKind::DeferredCreate,
        PlanEffectSummaryKind::DeferredReplace => EffectKind::DeferredReplace,
    }
}

fn write_auto_palette_css(out: &mut String) {
    let palette = &DARK_PALETTE;
    writeln!(out, "  <style>").unwrap();
    writeln!(out, "    @media (prefers-color-scheme: dark) {{").unwrap();
    for (class, property, color) in [
        ("fill-background", "fill", palette.background),
        ("fill-surface", "fill", palette.surface),
        ("fill-primary", "fill", palette.primary_text),
        ("fill-body", "fill", palette.body_text),
        ("fill-muted", "fill", palette.muted_text),
        ("fill-subtle", "fill", palette.subtle_text),
        ("fill-edge", "fill", palette.edge),
        ("stroke-edge", "stroke", palette.edge),
        ("stroke-border", "stroke", palette.border),
        ("stroke-divider", "stroke", palette.divider),
        ("stroke-unchanged", "stroke", palette.unchanged_border),
        ("fill-teal", "fill", palette.teal.color),
        ("stroke-teal", "stroke", palette.teal.color),
        ("fill-teal-tint", "fill", palette.teal.tint),
        ("fill-create", "fill", palette.create.color),
        ("stroke-create", "stroke", palette.create.color),
        ("fill-create-tint", "fill", palette.create.tint),
        ("fill-update", "fill", palette.update.color),
        ("stroke-update", "stroke", palette.update.color),
        ("fill-update-tint", "fill", palette.update.tint),
        ("fill-replace", "fill", palette.replace.color),
        ("stroke-replace", "stroke", palette.replace.color),
        ("fill-replace-tint", "fill", palette.replace.tint),
        ("fill-delete", "fill", palette.delete.color),
        ("stroke-delete", "stroke", palette.delete.color),
        ("fill-delete-tint", "fill", palette.delete.tint),
        ("fill-gray", "fill", palette.gray.color),
        ("stroke-gray", "stroke", palette.gray.color),
        ("fill-gray-tint", "fill", palette.gray.tint),
        ("fill-unchanged", "fill", palette.unchanged.color),
        ("stroke-unchanged-kind", "stroke", palette.unchanged.color),
        ("fill-unchanged-tint", "fill", palette.unchanged.tint),
    ] {
        writeln!(out, "      .{class} {{ {property}: {color}; }}").unwrap();
    }
    writeln!(
        out,
        "      .halo {{ stroke-opacity: {}; }}",
        palette.halo_opacity
    )
    .unwrap();
    writeln!(
        out,
        "      .unchanged-card {{ fill-opacity: {}; }}",
        palette.unchanged_opacity
    )
    .unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "  </style>").unwrap();
}

fn write_markers(out: &mut String, render_style: RenderStyle) {
    writeln!(out, "  <defs>").unwrap();
    writeln!(
        out,
        "    <marker id=\"arrow-default\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"4\" orient=\"auto\" markerUnits=\"strokeWidth\"><path{} d=\"M0,0 L8,4 L0,8 z\" fill=\"{}\"/></marker>",
        render_style.class("fill-edge"),
        render_style.palette.edge,
    )
    .unwrap();
    for blast in [BlastKind::Replace, BlastKind::Delete] {
        let id = blast.marker_id();
        let color = blast.color(render_style.palette);
        writeln!(
            out,
            "    <marker id=\"arrow-{id}\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"4\" orient=\"auto\" markerUnits=\"strokeWidth\"><path{} d=\"M0,0 L8,4 L0,8 z\" fill=\"{color}\"/></marker>",
            render_style.class(blast.fill_class(render_style.palette)),
        )
        .unwrap();
    }
    writeln!(out, "  </defs>").unwrap();
}

fn render_module_band_backgrounds(out: &mut String, layout: &Layout, render_style: RenderStyle) {
    let palette = render_style.palette;
    for rect in &layout.modules {
        writeln!(
            out,
            r#"    <rect{} x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="12" fill="{}" stroke="{}"/>"#,
            render_style.class("fill-surface stroke-border"),
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            palette.surface,
            palette.border
        )
        .unwrap();
    }
}

fn render_module_band_labels(
    out: &mut String,
    model: &GraphModel,
    layout: &Layout,
    render_style: RenderStyle,
) {
    let palette = render_style.palette;
    for rect in &layout.modules {
        let Some(module) = model.modules.iter().find(|module| module.id == rect.id) else {
            continue;
        };
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
        let label = format!("module \"{}\"{source}{collapsed}", module.label);
        writeln!(
            out,
            r#"    <rect{} x="{:.1}" y="{:.1}" width="{:.1}" height="21" rx="4" fill="{}"/>"#,
            render_style.class("fill-surface"),
            rect.x + 8.0,
            rect.y + 5.0,
            text_width(&label, 11.0) + 12.0,
            palette.surface,
        )
        .unwrap();
        writeln!(
            out,
            r#"    <text{} x="{:.1}" y="{:.1}" font-size="11" font-weight="600" fill="{}">module &quot;{}&quot;{}{}</text>"#,
            render_style.class("fill-muted"),
            rect.x + 14.0,
            rect.y + 20.0,
            palette.muted_text,
            escape(&module.label),
            escape(&source),
            collapsed
        )
        .unwrap();
    }
}

fn render_edges(out: &mut String, model: &GraphModel, layout: &Layout, render_style: RenderStyle) {
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
        let (color, color_class, marker, opacity, width) = match edge.highlight {
            Some(blast) => (
                blast.color(render_style.palette),
                blast.stroke_class(render_style.palette),
                blast.marker_id(),
                0.76,
                2.2,
            ),
            None => (
                render_style.palette.edge,
                "stroke-edge",
                "default",
                0.68,
                1.6,
            ),
        };
        let dash = if is_reference {
            " stroke-dasharray=\"6 5\""
        } else {
            ""
        };
        writeln!(
            out,
            "    <path{} d=\"{path}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"{width:.1}\" stroke-opacity=\"{opacity:.2}\"{dash} marker-end=\"url(#arrow-{marker})\"/>",
            render_style.class(color_class),
        )
        .unwrap();
    }
}

fn render_nodes(out: &mut String, model: &GraphModel, layout: &Layout, render_style: RenderStyle) {
    for rect in &layout.nodes {
        if let Some(node) = model.node(&rect.id) {
            render_node(out, node, rect, render_style);
        }
    }
}

fn render_node(out: &mut String, node: &GraphNode, rect: &NodeRect, render_style: RenderStyle) {
    let palette = render_style.palette;
    let style = kind_style(node.kind, palette);
    if let Some(halo) = node_halo(node) {
        let color = halo.color(palette);
        writeln!(
            out,
            "    <rect{} x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"14\" fill=\"none\" stroke=\"{color}\" stroke-width=\"{NODE_HALO_STROKE_WIDTH}\" stroke-opacity=\"{}\"/>",
            render_style.class(&format!("{} halo", halo.stroke_class(palette))),
            rect.x - NODE_HALO_OFFSET,
            rect.y - NODE_HALO_OFFSET,
            rect.width + NODE_HALO_OFFSET * 2.0,
            rect.height + NODE_HALO_OFFSET * 2.0,
            palette.halo_opacity,
        )
        .unwrap();
    }

    let (fill, border, fill_opacity, card_classes) = if node.kind == EffectKind::Unchanged {
        (
            palette.surface,
            palette.unchanged_border,
            palette.unchanged_opacity,
            "fill-surface stroke-unchanged unchanged-card".to_string(),
        )
    } else {
        (
            style.tint,
            style.color,
            "1",
            format!("{} {}", style.tint_class, style.stroke_class),
        )
    };
    let dash = if node.kind == EffectKind::Delete {
        " stroke-dasharray=\"6 4\""
    } else {
        ""
    };
    writeln!(out, "    <g>").unwrap();
    writeln!(
        out,
        "      <rect{} x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"9\" fill=\"{fill}\" fill-opacity=\"{fill_opacity}\" stroke=\"{border}\"{dash}/>",
        render_style.class(&card_classes),
        rect.x, rect.y, rect.width, rect.height
    )
    .unwrap();
    if node.kind != EffectKind::Unchanged {
        writeln!(
            out,
            "      <rect{} x=\"{:.1}\" y=\"{:.1}\" width=\"6\" height=\"{:.1}\" rx=\"3\" fill=\"{}\"/>",
            render_style.class(style.fill_class),
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
            "      <text{} x=\"{:.1}\" y=\"{:.1}\" font-size=\"13\" font-weight=\"700\" fill=\"{}\">{}</text>",
            render_style.class(style.fill_class),
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
        "      <rect{} x=\"{badge_x:.1}\" y=\"{:.1}\" width=\"{badge_width:.1}\" height=\"20\" rx=\"10\" fill=\"{}\" fill-opacity=\"0.10\"/>",
        render_style.class(style.fill_class),
        rect.y + 10.0,
        style.color
    )
    .unwrap();
    writeln!(
        out,
        "      <text{} x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" font-weight=\"700\" fill=\"{}\">{}</text>",
        render_style.class(style.fill_class),
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
        "      <text{} x=\"{text_x:.1}\" y=\"{label_y:.1}\" font-size=\"14\" font-weight=\"700\" fill=\"{}\"{decoration}>{}</text>",
        render_style.class("fill-primary"),
        palette.primary_text,
        escape(&truncate_middle(&node.label, MAX_LABEL_CHARS))
    )
    .unwrap();
    let type_y = if node.kind == EffectKind::Unchanged {
        rect.y + 61.0
    } else {
        rect.y + 67.0
    };
    writeln!(
        out,
        r#"      <text{} x="{text_x:.1}" y="{type_y:.1}" font-size="11" fill="{}">{}</text>"#,
        render_style.class("fill-subtle"),
        palette.subtle_text,
        escape(&truncate(&node.resource_type, MAX_TYPE_CHARS))
    )
    .unwrap();

    let mut cursor = type_y + 18.0;
    if let Some(discriminator) = &node.anonymous_discriminator {
        writeln!(
            out,
            r#"      <text{} x="{text_x:.1}" y="{cursor:.1}" font-size="10.5" fill="{}">anonymous #{}</text>"#,
            render_style.class("fill-muted"),
            palette.muted_text,
            escape(discriminator)
        )
        .unwrap();
        cursor += 17.0;
    }
    if let Some(note) = &node.note {
        writeln!(
            out,
            r#"      <text{} x="{text_x:.1}" y="{cursor:.1}" font-size="10.5" fill="{}">{}</text>"#,
            render_style.class("fill-muted"),
            palette.muted_text,
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
            "      <text{} x=\"{text_x:.1}\" y=\"{cursor:.1}\" font-size=\"10.5\" font-weight=\"600\" fill=\"{}\">{heading}</text>",
            render_style.class(style.fill_class),
            style.color
        )
        .unwrap();
        cursor += 16.0;
        for attribute in node.attribute_names.iter().take(MAX_ATTRIBUTE_NAMES) {
            writeln!(
                out,
                r#"      <text{} x="{:.1}" y="{cursor:.1}" font-size="10.5" fill="{}">• {}</text>"#,
                render_style.class("fill-body"),
                text_x + 8.0,
                palette.body_text,
                escape(&truncate(attribute, MAX_TYPE_CHARS))
            )
            .unwrap();
            cursor += 16.0;
        }
        if node.attribute_names.len() > MAX_ATTRIBUTE_NAMES {
            writeln!(
                out,
                r#"      <text{} x="{:.1}" y="{cursor:.1}" font-size="10.5" fill="{}">+{} more</text>"#,
                render_style.class("fill-subtle"),
                text_x + 8.0,
                palette.subtle_text,
                node.attribute_names.len() - MAX_ATTRIBUTE_NAMES
            )
            .unwrap();
        }
    }

    if !node.affected_by.is_empty() {
        let halo = node_halo(node).expect("affected node has a halo kind");
        let color = halo.color(palette);
        let affected = if node.affected_by.len() == 1 {
            format!("affected by {}", node.affected_by[0].label)
        } else {
            format!("affected by {} changes", node.affected_by.len())
        };
        writeln!(
            out,
            "      <text{} x=\"{text_x:.1}\" y=\"{:.1}\" font-size=\"10\" font-weight=\"600\" fill=\"{color}\">{}</text>",
            render_style.class(halo.fill_class(palette)),
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

fn render_legend(out: &mut String, model: &GraphModel, layout: &Layout, render_style: RenderStyle) {
    let palette = render_style.palette;
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
        EffectKind::Release,
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
        r#"    <line{} x1="28" y1="{divider_y:.1}" x2="{}" y2="{divider_y:.1}" stroke="{}"/>"#,
        render_style.class("stroke-divider"),
        layout.width - 28,
        palette.divider
    )
    .unwrap();
    let mut x = 28.0;
    let mut y = divider_y + 21.0;
    for kind in visible_kinds {
        let style = kind_style(kind, palette);
        let item_width = text_width(style.symbol, 11.0) + text_width(style.label, 10.0) + 28.0;
        if x + item_width > layout.width as f32 - 28.0 {
            x = 28.0;
            y += 20.0;
        }
        writeln!(
            out,
            "    <text{} x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"11\" font-weight=\"700\" fill=\"{}\">{}</text>",
            render_style.class(style.fill_class),
            style.color,
            escape(style.symbol)
        )
        .unwrap();
        let label_x = x + text_width(style.symbol, 11.0) + 6.0;
        writeln!(
            out,
            r#"    <text{} x="{label_x:.1}" y="{y:.1}" font-size="10" fill="{}">{}</text>"#,
            render_style.class("fill-muted"),
            palette.muted_text,
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
            "    <line{} x1=\"{semantics_x:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"1.6\" marker-end=\"url(#arrow-default)\"/><text{} x=\"{:.1}\" y=\"{semantics_y:.1}\" font-size=\"10\" fill=\"{}\">runs before</text>",
            render_style.class("stroke-edge"),
            semantics_y - 4.0,
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH,
            semantics_y - 4.0,
            palette.edge,
            render_style.class("fill-muted"),
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH + 10.0,
            palette.muted_text,
        )
        .unwrap();
        semantics_x += 146.0;
    }
    if has_reference_edges {
        writeln!(
            out,
            "    <line{} x1=\"{semantics_x:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"1.6\" stroke-dasharray=\"6 5\" marker-end=\"url(#arrow-default)\"/><text{} x=\"{:.1}\" y=\"{semantics_y:.1}\" font-size=\"10\" fill=\"{}\">references</text>",
            render_style.class("stroke-edge"),
            semantics_y - 4.0,
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH,
            semantics_y - 4.0,
            palette.edge,
            render_style.class("fill-muted"),
            semantics_x + LEGEND_EDGE_SAMPLE_WIDTH + 10.0,
            palette.muted_text,
        )
        .unwrap();
        semantics_x += 146.0;
    }
    if has_halo {
        let color = BlastKind::Replace.color(palette);
        writeln!(
            out,
            "    <rect{} x=\"{semantics_x:.1}\" y=\"{:.1}\" width=\"24\" height=\"13\" rx=\"4\" fill=\"none\" stroke=\"{color}\" stroke-width=\"5\" stroke-opacity=\"{}\"/><text{} x=\"{:.1}\" y=\"{semantics_y:.1}\" font-size=\"10\" fill=\"{}\">halo = downstream blast radius</text>",
            render_style.class(&format!(
                "{} halo",
                BlastKind::Replace.stroke_class(palette)
            )),
            semantics_y - 13.0,
            palette.halo_opacity,
            render_style.class("fill-muted"),
            semantics_x + 34.0,
            palette.muted_text,
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

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::collect_resolved([resource])
            .expect("fixture identity resolves")
            .remove(0)
    }

    fn fixture_svg_with_theme(name: &str, theme: SvgTheme) -> String {
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
        render_svg(&model, &layout, theme).text
    }

    fn fixture_svg(name: &str) -> String {
        fixture_svg_with_theme(name, SvgTheme::Fixed(Theme::Light))
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
    fn snapshot_state_only_directive_change_svg() {
        let svg = fixture_svg("state_only_directive_change");

        assert!(svg.contains("1 resource directive change"));
        assert!(!svg.contains(">No changes</tspan>"));
        insta::assert_snapshot!(svg);
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
            to: resolved(resource.clone()),
            changed_attributes: vec!["secret_token".to_string(), "write_only_token".to_string()],
        });
        let desired = vec![resolved(resource)];
        let summary = crate::plan_summary::PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        );
        let model = build_model(PlanImageInput {
            plan: &plan,
            desired_resources: &desired,
            unresolved_resources: &[],
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary: &summary,
        });
        let svg = render_svg(&model, &layout_graph(&model), SvgTheme::Fixed(Theme::Light)).text;

        assert!(svg.contains("secret_token"));
        assert!(svg.contains("write_only_token"));
        assert!(!svg.contains("SECRET-SENTINEL-3966"));
        assert!(!svg.contains("WRITE-ONLY-SENTINEL-3966"));
    }

    #[test]
    fn anonymous_cards_show_type_and_discriminator_without_attribute_values() {
        let svg = fixture_svg("plan_image_anonymous");

        assert_eq!(svg.matches(">Widget</text>").count(), 2);
        assert!(svg.contains("anonymous #3b61ad0d"));
        assert!(svg.contains("anonymous #980e2d2a"));
        assert!(!svg.contains("test_test_widget_"));
        assert!(!svg.contains("anonymous-one"));
        assert!(!svg.contains("anonymous-two"));

        let module_svg = fixture_svg("module_anonymous_resource");
        assert!(module_svg.contains(">bootstrap.RolePolicy</text>"));
        assert!(module_svg.contains("anonymous #968c7070"));
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
            to: resolved(resource.clone()),
            changed_attributes,
        });
        let desired = vec![resolved(resource)];
        let summary = crate::plan_summary::PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        );
        let model = build_model(PlanImageInput {
            plan: &plan,
            desired_resources: &desired,
            unresolved_resources: &[],
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary: &summary,
        });
        let svg = render_svg(&model, &layout_graph(&model), SvgTheme::Fixed(Theme::Light)).text;

        assert!(!svg.contains(long_label));
        assert!(svg.contains('…'));
        assert!(
            svg.contains("for_the_card"),
            "middle truncation should preserve the distinguishing tail of a named title"
        );
        assert!(svg.contains("• attribute_05"));
        assert!(!svg.contains("• attribute_06"));
        assert!(svg.contains("+2 more"));
    }

    #[test]
    fn svg_has_explicit_white_background_and_vendored_font_family() {
        let svg = fixture_svg("mixed_operations");
        assert!(svg.contains(&format!(
            r#"id="background" fill="{}""#,
            LIGHT_PALETTE.background
        )));
        assert!(svg.contains(FONT_FAMILY));
    }

    #[test]
    fn auto_svg_contains_both_palettes_and_dark_media_query() {
        let svg = fixture_svg_with_theme("plan_image_module_cascade", SvgTheme::Auto);

        assert!(svg.contains("@media (prefers-color-scheme: dark)"));
        assert!(svg.contains(LIGHT_PALETTE.background));
        assert!(svg.contains(DARK_PALETTE.background));
    }

    #[test]
    fn fixed_dark_svg_uses_no_light_background() {
        let svg = fixture_svg_with_theme("plan_image_module_cascade", SvgTheme::Fixed(Theme::Dark));

        assert!(svg.contains(DARK_PALETTE.background));
        assert!(!svg.contains(LIGHT_PALETTE.background));
        assert!(!svg.contains("prefers-color-scheme"));
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
        assert!(cascade.contains(">references</text>"));
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
        let resources = vec![a, connector, b];
        let mut parsed = carina_core::parser::ParsedFile::default();
        let graph =
            carina_core::resolved_graph::resolve_expanded_graph(&mut parsed, &resources, &[])
                .expect("test graph targets resolve");
        let desired = resources
            .into_iter()
            .map(|resource| {
                graph.resolve_resource(resource, &carina_core::schema::SchemaRegistry::new())
            })
            .collect::<Result<Vec<_>, _>>()
            .expect("test graph dependencies resolve");
        let mut plan = Plan::new();
        for resource in [desired[0].clone(), desired[2].clone()] {
            plan.add(Effect::Update {
                from: Box::new(State::existing(resource.id.clone(), HashMap::new())),
                identifier: AppliedIdentifier::new(format!(
                    "{}-id",
                    resource.binding.as_deref().expect("test resource is bound")
                ))
                .unwrap(),
                to: resource,
                changed_attributes: vec!["description".to_string()],
            });
        }
        let unresolved: Vec<_> = desired
            .iter()
            .map(|resource| UnresolvedResource::from_resolved_for_test(resource.as_inner().clone()))
            .collect();
        let summary = crate::plan_summary::PlanChangeSummary::new(
            &plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        );
        let model = build_model(PlanImageInput {
            plan: &plan,
            desired_resources: &desired,
            unresolved_resources: &unresolved,
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary: &summary,
        });
        let svg = render_svg(&model, &layout_graph(&model), SvgTheme::Fixed(Theme::Light)).text;

        assert!(svg.contains("stroke-dasharray=\"6 5\""));
        assert!(svg.contains(">references</text>"));
        assert!(!svg.contains(">runs before</text>"));
        let sample = svg
            .lines()
            .find(|line| {
                line.contains("stroke-dasharray=\"6 5\"") && line.contains(">references</text>")
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
    fn module_labels_overlay_reference_edges_with_opaque_theme_surface() {
        for (theme, surface) in [
            (SvgTheme::Fixed(Theme::Light), LIGHT_PALETTE.surface),
            (SvgTheme::Fixed(Theme::Dark), DARK_PALETTE.surface),
        ] {
            let svg = fixture_svg_with_theme("plan_image_reference_layout", theme);
            let label = "module &quot;app&quot; (./app-module)";
            let label_position = svg.find(label).expect("app module label");
            let dashed_edge_position = svg
                .match_indices("<path d=\"M")
                .map(|(position, _)| position)
                .filter(|position| {
                    svg[*position..]
                        .lines()
                        .next()
                        .is_some_and(|line| line.contains("stroke-dasharray=\"6 5\""))
                })
                .max()
                .expect("dashed reference edge");
            assert!(
                dashed_edge_position < label_position,
                "module label must be painted after reference edges"
            );
            let overlay = &svg[dashed_edge_position..label_position];
            assert!(
                overlay.contains("<rect x=\"") && overlay.contains(&format!("fill=\"{surface}\"")),
                "module label needs an opaque background matching the band surface"
            );
        }
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

    #[test]
    fn snapshot_module_cascade_dark_svg() {
        insta::assert_snapshot!(fixture_svg_with_theme(
            "plan_image_module_cascade",
            SvgTheme::Fixed(Theme::Dark),
        ));
    }

    #[test]
    fn snapshot_module_cascade_auto_svg() {
        insta::assert_snapshot!(fixture_svg_with_theme(
            "plan_image_module_cascade",
            SvgTheme::Auto,
        ));
    }

    #[test]
    fn snapshot_reference_layout_svg() {
        insta::assert_snapshot!(fixture_svg("plan_image_reference_layout"));
    }

    #[test]
    fn snapshot_anonymous_svg() {
        insta::assert_snapshot!(fixture_svg("plan_image_anonymous"));
    }

    #[test]
    fn snapshot_anonymous_loop_origin_labels_in_plan_image() {
        let fixture =
            crate::fixture_plan::build_plan_from_fixture_name("anonymous_loop_insert_before");
        let summary = crate::fixture_plan::build_fixture_plan_change_summary(&fixture);
        let model = build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &fixture.desired_resources,
            unresolved_resources: &fixture.unresolved_resources,
            compositions: &fixture.compositions,
            expansion_trace: Some(&fixture.expansion_trace),
            state: fixture.state_file.as_ref().map(|state| StateMetadata {
                lineage: &state.lineage,
                serial: state.serial,
            }),
            summary: &summary,
        });
        let mut labels = model
            .nodes
            .iter()
            .filter_map(|node| node.label.contains("(for [").then_some(node.label.clone()))
            .collect::<Vec<_>>();
        labels.sort();

        insta::assert_debug_snapshot!(labels, @r###"
        [
            "Vpc  (for [0] at main.crn:5)",
            "awscc_ec2_vpc_0bc2a2bb  (for [0] at main.crn:11)",
            "awscc_ec2_vpc_23be212c  (for [1] at main.crn:11)",
        ]
        "###);
    }
}
