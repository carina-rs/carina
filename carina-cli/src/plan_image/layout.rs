use std::collections::{BTreeSet, HashMap, HashSet};

use carina_core::resource::EphemeralId;

use super::model::{EffectKind, GraphModel, GraphNode, GraphNodeId};

/// JetBrains Mono advances every glyph by 600 units per 1000-unit em.
pub(crate) const MONOSPACE_ADVANCE_EM: f32 = 0.6;
pub(crate) const MAX_ATTRIBUTE_NAMES: usize = 6;
pub(crate) const MAX_LABEL_CHARS: usize = 34;
pub(crate) const MAX_TYPE_CHARS: usize = 42;

const HEADER_HEIGHT: f32 = 112.0;
const LEGEND_HEIGHT: f32 = 92.0;
const CANVAS_SIDE_MARGIN: f32 = 28.0;
const COLUMN_GAP: f32 = 104.0;
const NODE_GAP: f32 = 18.0;
const SAME_COLUMN_REFERENCE_GAP: f32 = NODE_GAP * 3.0;
const BAND_GAP: f32 = 22.0;
const BAND_PADDING: f32 = 16.0;
pub(crate) const NODE_HALO_OFFSET: f32 = 5.0;
pub(crate) const NODE_HALO_STROKE_WIDTH: f32 = 8.0;
const NODE_HALO_OUTSET: f32 = NODE_HALO_OFFSET + NODE_HALO_STROKE_WIDTH / 2.0;
const MIN_HALO_BAND_MARGIN: f32 = 4.0;
const _: () = assert!(BAND_PADDING >= NODE_HALO_OUTSET + MIN_HALO_BAND_MARGIN);
const MODULE_LABEL_HEIGHT: f32 = 30.0;
const MIN_NODE_WIDTH: f32 = 220.0;
const MAX_NODE_WIDTH: f32 = 344.0;
const MIN_CANVAS_WIDTH: f32 = 900.0;
const DUMMY_HEIGHT: f32 = 6.0;
const DUMMY_COLUMN_WIDTH: f32 = 24.0;
const BARYCENTER_SWEEPS: usize = 4;
const SWAP_REFINEMENT_SWEEPS: usize = 8;
const RELAXATION_ITERATIONS: usize = 5;
const MAX_RELAXATION_SHIFT: f32 = 24.0;
const PORT_MARGIN_FRACTION: f32 = 0.2;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeRect {
    pub id: GraphNodeId,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModuleRect {
    pub id: EphemeralId,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EdgeRoute {
    pub from: GraphNodeId,
    pub to: GraphNodeId,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CardSide {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy)]
enum RouteEndpoint {
    Start,
    End,
}

#[derive(Debug)]
struct PortEndpoint {
    route_index: usize,
    endpoint: RouteEndpoint,
    other_primary: f32,
    other_secondary: f32,
    stable_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Layout {
    pub width: u32,
    pub height: u32,
    pub nodes: Vec<NodeRect>,
    pub modules: Vec<ModuleRect>,
    pub edges: Vec<EdgeRoute>,
    node_indices: HashMap<GraphNodeId, usize>,
    edge_indices: HashMap<(GraphNodeId, GraphNodeId), usize>,
}

impl Layout {
    pub(crate) fn node(&self, id: &GraphNodeId) -> Option<&NodeRect> {
        self.node_indices.get(id).map(|index| &self.nodes[*index])
    }

    pub(crate) fn edge_route(&self, from: &GraphNodeId, to: &GraphNodeId) -> Option<&EdgeRoute> {
        self.edge_indices
            .get(&(from.clone(), to.clone()))
            .map(|index| &self.edges[*index])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LayoutVertexId {
    Real(GraphNodeId),
    Dummy {
        from: GraphNodeId,
        to: GraphNodeId,
        column: usize,
    },
}

impl LayoutVertexId {
    fn stable_key(&self) -> String {
        match self {
            Self::Real(id) => format!("real:{}", id.stable_key()),
            Self::Dummy { from, to, column } => format!(
                "dummy:{}:{}:{column:020}",
                from.stable_key(),
                to.stable_key()
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PositionedVertex {
    id: LayoutVertexId,
    column: usize,
    center_y: f32,
    height: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct BandBounds {
    id: BandId,
    content_top: f32,
    content_bottom: f32,
}

#[derive(Debug, Clone)]
struct ExpandedGraph {
    columns: Vec<Vec<LayoutVertexId>>,
    edges: Vec<(LayoutVertexId, LayoutVertexId)>,
    routes: Vec<ExpandedRoute>,
    bands: HashMap<LayoutVertexId, BandId>,
    same_column_reference_pairs: HashSet<(LayoutVertexId, LayoutVertexId)>,
}

#[derive(Debug, Clone)]
struct ExpandedRoute {
    from: GraphNodeId,
    to: GraphNodeId,
    vertices: Vec<LayoutVertexId>,
    is_reference: bool,
}

#[derive(Debug, Clone)]
struct OrientedEdge {
    actual_from: GraphNodeId,
    actual_to: GraphNodeId,
    from: GraphNodeId,
    to: GraphNodeId,
    reversed: bool,
    is_reference: bool,
}

#[derive(Debug, Clone)]
struct Layering {
    layers: HashMap<GraphNodeId, usize>,
    edges: Vec<OrientedEdge>,
}

pub(crate) fn layout_graph(model: &GraphModel) -> Layout {
    let layering = graph_layering(model);
    let bands = ordered_bands(model);
    let band_rank: HashMap<_, _> = bands
        .iter()
        .cloned()
        .enumerate()
        .map(|(rank, band)| (band, rank))
        .collect();
    let mut expanded = expand_long_edges(model, &layering);
    sort_initial_columns(model, &band_rank, &mut expanded);
    reduce_crossings(model, &bands, &mut expanded);
    refine_crossings(&bands, &mut expanded);

    let column_count = expanded.columns.len();
    let column_widths: Vec<_> = expanded
        .columns
        .iter()
        .map(|column| column_width(model, column))
        .collect();
    let graph_width =
        column_widths.iter().sum::<f32>() + COLUMN_GAP * column_count.saturating_sub(1) as f32;
    let width = MIN_CANVAS_WIDTH.max(graph_width + CANVAS_SIDE_MARGIN * 2.0);
    let graph_left = CANVAS_SIDE_MARGIN;
    let mut column_x = Vec::with_capacity(column_count);
    let mut x = graph_left;
    for column_width in &column_widths {
        column_x.push(x);
        x += *column_width + COLUMN_GAP;
    }

    let mut band_heights = Vec::with_capacity(bands.len());
    for band in &bands {
        let max_stack = expanded
            .columns
            .iter()
            .map(|column| stack_height(model, &expanded, column, band))
            .fold(0.0_f32, f32::max);
        let label_height = band_label_height(band);
        band_heights.push((label_height + max_stack + BAND_PADDING * 2.0).max(58.0));
    }

    let mut band_tops = Vec::with_capacity(bands.len());
    let mut cursor_y = HEADER_HEIGHT;
    for height in &band_heights {
        band_tops.push(cursor_y);
        cursor_y += *height + BAND_GAP;
    }

    let band_bounds: Vec<_> = bands
        .iter()
        .enumerate()
        .map(|(index, band)| BandBounds {
            id: band.clone(),
            content_top: band_tops[index] + BAND_PADDING + band_label_height(band),
            content_bottom: band_tops[index] + band_heights[index] - BAND_PADDING,
        })
        .collect();

    let mut vertices = Vec::new();
    for (column_idx, column) in expanded.columns.iter().enumerate() {
        for (band_idx, band) in bands.iter().enumerate() {
            let band_vertices: Vec<_> = column
                .iter()
                .filter(|id| expanded.bands.get(*id) == Some(band))
                .collect();
            if band_vertices.is_empty() {
                continue;
            }
            let stack = stack_height(model, &expanded, column, band);
            let bounds = &band_bounds[band_idx];
            let mut y = bounds.content_top
                + ((bounds.content_bottom - bounds.content_top) - stack).max(0.0) / 2.0;
            for (index, id) in band_vertices.iter().enumerate() {
                let height = vertex_height(model, id);
                vertices.push(PositionedVertex {
                    id: (*id).clone(),
                    column: column_idx,
                    center_y: y + height / 2.0,
                    height,
                });
                y += height;
                if let Some(next) = band_vertices.get(index + 1) {
                    y += vertex_gap(&expanded, id, next);
                }
            }
        }
    }
    relax_coordinates(&expanded, &band_bounds, &mut vertices);

    let vertex_positions: HashMap<_, _> = vertices
        .iter()
        .map(|vertex| (vertex.id.clone(), vertex.center_y))
        .collect();
    let mut positioned_nodes = Vec::with_capacity(model.nodes.len());
    for vertex in &vertices {
        let LayoutVertexId::Real(id) = &vertex.id else {
            continue;
        };
        let node = model.node(id).expect("real layout vertex comes from model");
        positioned_nodes.push(NodeRect {
            id: id.clone(),
            x: column_x[vertex.column],
            y: vertex.center_y - vertex.height / 2.0,
            width: graph_node_width(node),
            height: vertex.height,
            column: vertex.column,
        });
    }
    positioned_nodes.sort_by(|left, right| {
        left.column
            .cmp(&right.column)
            .then_with(|| left.y.total_cmp(&right.y))
            .then_with(|| left.id.stable_key().cmp(&right.id.stable_key()))
    });

    let edge_routes = build_edge_routes(
        &expanded,
        &positioned_nodes,
        &vertex_positions,
        &column_x,
        &column_widths,
    );

    let mut module_rects = Vec::new();
    for (band_idx, band) in bands.iter().enumerate() {
        if let BandId::Module(id) = band {
            let members: Vec<_> = positioned_nodes
                .iter()
                .filter(|rect| {
                    model
                        .node(&rect.id)
                        .is_some_and(|node| node.module.as_ref() == Some(id))
                })
                .collect();
            let Some(member_left) = members.iter().map(|rect| rect.x).reduce(f32::min) else {
                continue;
            };
            let member_right = members
                .iter()
                .map(|rect| rect.x + rect.width)
                .reduce(f32::max)
                .expect("module band has at least one member");
            module_rects.push(ModuleRect {
                id: id.clone(),
                x: member_left - BAND_PADDING,
                y: band_tops[band_idx],
                width: member_right - member_left + BAND_PADDING * 2.0,
                height: band_heights[band_idx],
            });
        }
    }

    let graph_bottom = if bands.is_empty() {
        HEADER_HEIGHT + 24.0
    } else {
        cursor_y - BAND_GAP
    };
    let node_indices = positioned_nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect();
    let edge_indices = edge_routes
        .iter()
        .enumerate()
        .map(|(index, edge)| ((edge.from.clone(), edge.to.clone()), index))
        .collect();
    Layout {
        width: width.ceil() as u32,
        height: (graph_bottom + LEGEND_HEIGHT).ceil() as u32,
        nodes: positioned_nodes,
        modules: module_rects,
        edges: edge_routes,
        node_indices,
        edge_indices,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BandId {
    Root,
    Module(EphemeralId),
}

impl BandId {
    fn for_node(node: &GraphNode) -> Self {
        node.module.clone().map(Self::Module).unwrap_or(Self::Root)
    }
}

fn ordered_bands(model: &GraphModel) -> Vec<BandId> {
    let mut bands = Vec::new();
    if model.nodes.iter().any(|node| node.module.is_none()) {
        bands.push(BandId::Root);
    }
    bands.extend(
        model
            .modules
            .iter()
            .filter(|module| {
                model
                    .nodes
                    .iter()
                    .any(|node| node.module.as_ref() == Some(&module.id))
            })
            .map(|module| BandId::Module(module.id.clone())),
    );
    bands
}

fn graph_node_width(node: &GraphNode) -> f32 {
    let mut widest = measured_width(&truncate_middle(&node.label, MAX_LABEL_CHARS), 15.0) + 70.0;
    widest = widest.max(
        measured_width(
            &truncate_for_measurement(&node.resource_type, MAX_TYPE_CHARS),
            11.5,
        ) + 34.0,
    );
    if let Some(discriminator) = &node.anonymous_discriminator {
        widest = widest.max(measured_width(&format!("anonymous #{discriminator}"), 10.5) + 34.0);
    }
    if let Some(note) = &node.note {
        widest = widest.max(measured_width(note, 11.0) + 34.0);
    }
    for attribute in node.attribute_names.iter().take(MAX_ATTRIBUTE_NAMES) {
        widest = widest.max(measured_width(attribute, 11.0) + 52.0);
    }
    if !node.affected_by.is_empty() {
        let text = if node.affected_by.len() == 1 {
            format!("affected by {}", node.affected_by[0].label)
        } else {
            format!("affected by {} changes", node.affected_by.len())
        };
        widest = widest.max(measured_width(&text, 10.0) + 34.0);
    }
    widest.clamp(MIN_NODE_WIDTH, MAX_NODE_WIDTH).ceil()
}

pub(crate) fn graph_node_height(node: &GraphNode) -> f32 {
    let mut height: f32 = if node.kind == EffectKind::Unchanged {
        66.0
    } else {
        78.0
    };
    if node.anonymous_discriminator.is_some() {
        height += 17.0;
    }
    if node.note.is_some() {
        height += 17.0;
    }
    if matches!(node.kind, EffectKind::Update | EffectKind::Replace)
        && !node.attribute_names.is_empty()
    {
        let shown = node.attribute_names.len().min(MAX_ATTRIBUTE_NAMES);
        let more = usize::from(node.attribute_names.len() > MAX_ATTRIBUTE_NAMES);
        height += (shown + more + 1) as f32 * 16.0 + 3.0;
    }
    if !node.affected_by.is_empty() {
        height += 20.0;
    }
    height
}

fn measured_width(text: &str, font_size: f32) -> f32 {
    text.chars().count() as f32 * font_size * MONOSPACE_ADVANCE_EM
}

fn truncate_for_measurement(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars()
        .take(max_chars.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

pub(crate) fn truncate_middle(text: &str, max_chars: usize) -> String {
    let characters = text.chars().collect::<Vec<_>>();
    if characters.len() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }

    let kept = max_chars - 1;
    let prefix_len = kept.div_ceil(2);
    let suffix_len = kept - prefix_len;
    characters[..prefix_len]
        .iter()
        .copied()
        .chain(std::iter::once('…'))
        .chain(characters[characters.len() - suffix_len..].iter().copied())
        .collect()
}

fn band_label_height(band: &BandId) -> f32 {
    match band {
        BandId::Root => 0.0,
        BandId::Module(_) => MODULE_LABEL_HEIGHT,
    }
}

fn vertex_height(model: &GraphModel, id: &LayoutVertexId) -> f32 {
    match id {
        LayoutVertexId::Real(id) => model.node(id).map_or(DUMMY_HEIGHT, graph_node_height),
        LayoutVertexId::Dummy { .. } => DUMMY_HEIGHT,
    }
}

fn column_width(model: &GraphModel, column: &[LayoutVertexId]) -> f32 {
    column
        .iter()
        .filter_map(|id| match id {
            LayoutVertexId::Real(id) => model.node(id),
            LayoutVertexId::Dummy { .. } => None,
        })
        .map(graph_node_width)
        .fold(DUMMY_COLUMN_WIDTH, f32::max)
}

fn stack_height(
    model: &GraphModel,
    expanded: &ExpandedGraph,
    column: &[LayoutVertexId],
    band: &BandId,
) -> f32 {
    let vertices: Vec<_> = column
        .iter()
        .filter(|id| expanded.bands.get(*id) == Some(band))
        .collect();
    let heights = vertices
        .iter()
        .map(|id| vertex_height(model, id))
        .sum::<f32>();
    let gaps = vertices
        .windows(2)
        .map(|pair| vertex_gap(expanded, pair[0], pair[1]))
        .sum::<f32>();
    heights + gaps
}

fn vertex_gap(expanded: &ExpandedGraph, upper: &LayoutVertexId, lower: &LayoutVertexId) -> f32 {
    if expanded
        .same_column_reference_pairs
        .contains(&(upper.clone(), lower.clone()))
    {
        SAME_COLUMN_REFERENCE_GAP
    } else {
        NODE_GAP
    }
}

fn expand_long_edges(model: &GraphModel, layering: &Layering) -> ExpandedGraph {
    let layers = &layering.layers;
    let column_count = layers.values().copied().max().map_or(0, |max| max + 1);
    let mut columns = vec![Vec::new(); column_count];
    let mut bands = HashMap::new();
    for node in &model.nodes {
        let Some(column) = layers.get(&node.id).copied() else {
            continue;
        };
        let id = LayoutVertexId::Real(node.id.clone());
        columns[column].push(id.clone());
        bands.insert(id, BandId::for_node(node));
    }

    let mut edges = Vec::new();
    let mut routes = Vec::new();
    let mut same_column_reference_pairs = HashSet::new();
    for edge in &layering.edges {
        let (Some(from_column), Some(to_column)) = (
            layers.get(&edge.from).copied(),
            layers.get(&edge.to).copied(),
        ) else {
            continue;
        };
        let from = LayoutVertexId::Real(edge.from.clone());
        let to = LayoutVertexId::Real(edge.to.clone());
        let from_band = bands.get(&from).cloned().unwrap_or(BandId::Root);
        let to_band = bands.get(&to).cloned().unwrap_or(BandId::Root);
        let mut route = vec![from.clone()];
        let short_reference = edge.is_reference
            && (from_column == to_column || (edge.reversed && from_column + 1 == to_column));
        if edge.is_reference && from_column == to_column {
            same_column_reference_pairs.insert((from.clone(), to.clone()));
            same_column_reference_pairs.insert((to.clone(), from.clone()));
        }
        if !short_reference && to_column > from_column + 1 {
            let span = to_column - from_column;
            for (column, column_vertices) in columns
                .iter_mut()
                .enumerate()
                .take(to_column)
                .skip(from_column + 1)
            {
                let dummy = LayoutVertexId::Dummy {
                    from: edge.actual_from.clone(),
                    to: edge.actual_to.clone(),
                    column,
                };
                let band = if from_band == to_band || (column - from_column) * 2 <= span {
                    from_band.clone()
                } else {
                    to_band.clone()
                };
                column_vertices.push(dummy.clone());
                bands.insert(dummy.clone(), band);
                route.push(dummy);
            }
        }
        route.push(to.clone());
        for pair in route.windows(2) {
            edges.push((pair[0].clone(), pair[1].clone()));
        }
        if edge.reversed {
            route.reverse();
        }
        routes.push(ExpandedRoute {
            from: edge.actual_from.clone(),
            to: edge.actual_to.clone(),
            vertices: route,
            is_reference: edge.is_reference,
        });
    }
    routes.sort_by_key(|route| (route.from.stable_key(), route.to.stable_key()));

    ExpandedGraph {
        columns,
        edges,
        routes,
        bands,
        same_column_reference_pairs,
    }
}

fn sort_initial_columns(
    model: &GraphModel,
    band_rank: &HashMap<BandId, usize>,
    expanded: &mut ExpandedGraph,
) {
    for column in &mut expanded.columns {
        column.sort_by_key(|id| {
            (
                band_rank.get(&expanded.bands[id]).copied().unwrap_or(0),
                layout_vertex_sort_key(model, id),
            )
        });
    }
}

fn cycle_removed_ordering_edges(
    model: &GraphModel,
) -> (Vec<OrientedEdge>, HashSet<(GraphNodeId, GraphNodeId)>) {
    let node_ids: HashSet<_> = model
        .nodes
        .iter()
        .filter(|node| node.kind != EffectKind::Unchanged)
        .map(|node| node.id.clone())
        .collect();
    let mut outgoing: HashMap<GraphNodeId, Vec<GraphNodeId>> = HashMap::new();
    for edge in &model.ordering_edges {
        if node_ids.contains(&edge.from) && node_ids.contains(&edge.to) {
            outgoing
                .entry(edge.from.clone())
                .or_default()
                .push(edge.to.clone());
        }
    }
    for targets in outgoing.values_mut() {
        targets.sort_by_key(|id| layout_node_sort_key(model, id));
        targets.dedup();
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Visiting,
        Done,
    }

    fn visit(
        id: &GraphNodeId,
        outgoing: &HashMap<GraphNodeId, Vec<GraphNodeId>>,
        states: &mut HashMap<GraphNodeId, VisitState>,
        reversed: &mut HashSet<(GraphNodeId, GraphNodeId)>,
    ) {
        states.insert(id.clone(), VisitState::Visiting);
        if let Some(targets) = outgoing.get(id) {
            for target in targets {
                match states.get(target) {
                    Some(VisitState::Visiting) => {
                        reversed.insert((id.clone(), target.clone()));
                    }
                    Some(VisitState::Done) => {}
                    None => visit(target, outgoing, states, reversed),
                }
            }
        }
        states.insert(id.clone(), VisitState::Done);
    }

    let mut ordered_nodes: Vec<_> = node_ids.iter().cloned().collect();
    ordered_nodes.sort_by_key(|id| layout_node_sort_key(model, id));
    let mut states = HashMap::new();
    let mut reversed_edges = HashSet::new();
    for id in ordered_nodes {
        if !states.contains_key(&id) {
            visit(&id, &outgoing, &mut states, &mut reversed_edges);
        }
    }

    let edges: Vec<_> = model
        .ordering_edges
        .iter()
        .filter(|edge| node_ids.contains(&edge.from) && node_ids.contains(&edge.to))
        .map(|edge| {
            let reversed = reversed_edges.contains(&(edge.from.clone(), edge.to.clone()));
            let (from, to) = if reversed {
                (edge.to.clone(), edge.from.clone())
            } else {
                (edge.from.clone(), edge.to.clone())
            };
            OrientedEdge {
                actual_from: edge.from.clone(),
                actual_to: edge.to.clone(),
                from,
                to,
                reversed,
                is_reference: false,
            }
        })
        .collect();
    (edges, reversed_edges)
}

fn cycle_removed_layering(model: &GraphModel) -> Layering {
    let (edges, _) = cycle_removed_ordering_edges(model);
    let layers = longest_path_layers(model, &edges);
    Layering { layers, edges }
}

fn graph_layering(model: &GraphModel) -> Layering {
    let mut layering = cycle_removed_layering(model);
    let mut constraint_edges = layering.edges.clone();
    let mut accepted: HashMap<GraphNodeId, Vec<GraphNodeId>> = HashMap::new();
    for edge in &constraint_edges {
        accepted
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }
    for targets in accepted.values_mut() {
        targets.sort_by_key(GraphNodeId::stable_key);
        targets.dedup();
    }

    let visible: HashSet<_> = model.nodes.iter().map(|node| node.id.clone()).collect();
    let mut references = model.reference_edges.clone();
    references.sort_by_key(|edge| (edge.from.stable_key(), edge.to.stable_key()));
    // Ordering constraints are already present in `accepted`. Add references
    // deterministically only while the constraint graph remains acyclic.
    for edge in &references {
        if !visible.contains(&edge.from)
            || !visible.contains(&edge.to)
            || path_exists(&accepted, &edge.to, &edge.from)
        {
            continue;
        }
        let targets = accepted.entry(edge.from.clone()).or_default();
        targets.push(edge.to.clone());
        targets.sort_by_key(GraphNodeId::stable_key);
        targets.dedup();
        constraint_edges.push(OrientedEdge {
            actual_from: edge.from.clone(),
            actual_to: edge.to.clone(),
            from: edge.from.clone(),
            to: edge.to.clone(),
            reversed: false,
            is_reference: true,
        });
    }
    layering.layers = longest_path_layers(model, &constraint_edges);

    for edge in references {
        let (Some(from_column), Some(to_column)) = (
            layering.layers.get(&edge.from).copied(),
            layering.layers.get(&edge.to).copied(),
        ) else {
            continue;
        };
        let reversed = from_column > to_column;
        let (from, to) = if reversed {
            (edge.to.clone(), edge.from.clone())
        } else {
            (edge.from.clone(), edge.to.clone())
        };
        layering.edges.push(OrientedEdge {
            actual_from: edge.from.clone(),
            actual_to: edge.to.clone(),
            from,
            to,
            reversed,
            is_reference: true,
        });
    }
    layering
        .edges
        .sort_by_key(|edge| (edge.actual_from.stable_key(), edge.actual_to.stable_key()));
    layering
}

fn path_exists(
    adjacency: &HashMap<GraphNodeId, Vec<GraphNodeId>>,
    from: &GraphNodeId,
    to: &GraphNodeId,
) -> bool {
    let mut stack = vec![from.clone()];
    let mut visited = HashSet::new();
    while let Some(id) = stack.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if id == *to {
            return true;
        }
        if let Some(adjacent) = adjacency.get(&id) {
            stack.extend(adjacent.iter().cloned());
        }
    }
    false
}

fn longest_path_layers(model: &GraphModel, edges: &[OrientedEdge]) -> HashMap<GraphNodeId, usize> {
    let node_ids: HashSet<_> = model.nodes.iter().map(|node| node.id.clone()).collect();
    let mut indegree: HashMap<_, usize> = node_ids.iter().cloned().map(|id| (id, 0)).collect();
    let mut outgoing: HashMap<GraphNodeId, Vec<(GraphNodeId, usize)>> = HashMap::new();
    let mut unique_edges = HashSet::new();
    for edge in edges {
        if unique_edges.insert((edge.from.clone(), edge.to.clone())) {
            // References prevent right-to-left placement but may share a
            // column. Ordering edges must advance by at least one column.
            let column_advance = usize::from(!edge.is_reference);
            outgoing
                .entry(edge.from.clone())
                .or_default()
                .push((edge.to.clone(), column_advance));
            *indegree.entry(edge.to.clone()).or_default() += 1;
        }
    }
    for targets in outgoing.values_mut() {
        targets.sort_by_key(|(id, _)| id.stable_key());
        targets.dedup();
    }

    let ready_nodes: Vec<_> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let node_by_key: HashMap<_, _> = node_ids
        .iter()
        .map(|id| (layout_node_sort_key(model, id), id.clone()))
        .collect();
    let mut ready: BTreeSet<_> = ready_nodes
        .iter()
        .map(|id| layout_node_sort_key(model, id))
        .collect();
    let mut layers: HashMap<_, usize> = node_ids.iter().cloned().map(|id| (id, 0)).collect();
    let mut processed = HashSet::new();
    while let Some(key) = ready.pop_first() {
        let id = node_by_key[&key].clone();
        if !processed.insert(id.clone()) {
            continue;
        }
        let parent_layer = layers[&id];
        if let Some(targets) = outgoing.get(&id) {
            for (target, minimum_advance) in targets {
                layers
                    .entry(target.clone())
                    .and_modify(|layer| *layer = (*layer).max(parent_layer + minimum_advance));
                let degree = indegree
                    .get_mut(target)
                    .expect("edge target initialized in indegree map");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(layout_node_sort_key(model, target));
                }
            }
        }
    }

    debug_assert_eq!(processed.len(), node_ids.len());
    layers
}

fn layout_node_sort_key(model: &GraphModel, id: &GraphNodeId) -> (String, String, String) {
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
}

fn layout_vertex_sort_key(model: &GraphModel, id: &LayoutVertexId) -> (String, String, String) {
    match id {
        LayoutVertexId::Real(id) => layout_node_sort_key(model, id),
        LayoutVertexId::Dummy { .. } => (String::new(), String::new(), id.stable_key()),
    }
}

fn reduce_crossings(model: &GraphModel, bands: &[BandId], expanded: &mut ExpandedGraph) {
    let mut predecessors: HashMap<LayoutVertexId, Vec<LayoutVertexId>> = HashMap::new();
    let mut successors: HashMap<LayoutVertexId, Vec<LayoutVertexId>> = HashMap::new();
    for (from, to) in &expanded.edges {
        predecessors
            .entry(to.clone())
            .or_default()
            .push(from.clone());
        successors.entry(from.clone()).or_default().push(to.clone());
    }
    for neighbors in predecessors.values_mut().chain(successors.values_mut()) {
        neighbors.sort_by_key(LayoutVertexId::stable_key);
        neighbors.dedup();
    }

    let vertex_bands = &expanded.bands;
    let columns = &mut expanded.columns;
    for _ in 0..BARYCENTER_SWEEPS {
        for column_idx in 1..columns.len() {
            reorder_column(
                model,
                bands,
                vertex_bands,
                columns,
                column_idx,
                &predecessors,
            );
        }
        for column_idx in (0..columns.len().saturating_sub(1)).rev() {
            reorder_column(model, bands, vertex_bands, columns, column_idx, &successors);
        }
    }
}

fn reorder_column(
    model: &GraphModel,
    bands: &[BandId],
    vertex_bands: &HashMap<LayoutVertexId, BandId>,
    columns: &mut [Vec<LayoutVertexId>],
    column_idx: usize,
    neighbors: &HashMap<LayoutVertexId, Vec<LayoutVertexId>>,
) {
    let positions: HashMap<_, _> = columns
        .iter()
        .flat_map(|column| column.iter().enumerate())
        .map(|(position, id)| (id.clone(), position))
        .collect();
    let previous = columns[column_idx].clone();
    let previous_rank: HashMap<_, _> = previous
        .iter()
        .enumerate()
        .map(|(rank, id)| (id.clone(), rank))
        .collect();
    let mut reordered = Vec::with_capacity(previous.len());
    for band in bands {
        let mut group: Vec<_> = previous
            .iter()
            .filter(|id| vertex_bands.get(*id) == Some(band))
            .cloned()
            .collect();
        group.sort_by(|left, right| {
            let left_barycenter = barycenter(left, neighbors, &positions);
            let right_barycenter = barycenter(right, neighbors, &positions);
            left_barycenter
                .total_cmp(&right_barycenter)
                .then_with(|| previous_rank[left].cmp(&previous_rank[right]))
                .then_with(|| {
                    layout_vertex_sort_key(model, left).cmp(&layout_vertex_sort_key(model, right))
                })
        });
        reordered.extend(group);
    }
    debug_assert_eq!(reordered.len(), previous.len());
    columns[column_idx] = reordered;
}

fn barycenter(
    id: &LayoutVertexId,
    neighbors: &HashMap<LayoutVertexId, Vec<LayoutVertexId>>,
    positions: &HashMap<LayoutVertexId, usize>,
) -> f32 {
    let Some(neighbors) = neighbors.get(id) else {
        return f32::INFINITY;
    };
    let positions: Vec<_> = neighbors
        .iter()
        .filter_map(|neighbor| positions.get(neighbor).copied())
        .collect();
    if positions.is_empty() {
        f32::INFINITY
    } else {
        positions.iter().sum::<usize>() as f32 / positions.len() as f32
    }
}

#[cfg(test)]
fn crossing_count(
    columns: &[Vec<LayoutVertexId>],
    edges: &[(LayoutVertexId, LayoutVertexId)],
) -> usize {
    let locations: HashMap<_, _> = columns
        .iter()
        .enumerate()
        .flat_map(|(column, vertices)| {
            vertices
                .iter()
                .enumerate()
                .map(move |(position, id)| (id.clone(), (column, position)))
        })
        .collect();
    let mut crossings = 0;
    for (index, (left_from, left_to)) in edges.iter().enumerate() {
        let (
            Some(&(left_from_column, left_from_position)),
            Some(&(left_to_column, left_to_position)),
        ) = (locations.get(left_from), locations.get(left_to))
        else {
            continue;
        };
        if left_to_column != left_from_column + 1 {
            continue;
        }
        for (right_from, right_to) in &edges[index + 1..] {
            if left_from == right_from || left_to == right_to {
                continue;
            }
            let (
                Some(&(right_from_column, right_from_position)),
                Some(&(right_to_column, right_to_position)),
            ) = (locations.get(right_from), locations.get(right_to))
            else {
                continue;
            };
            if right_from_column != left_from_column || right_to_column != left_to_column {
                continue;
            }
            if (left_from_position < right_from_position) != (left_to_position < right_to_position)
            {
                crossings += 1;
            }
        }
    }
    crossings
}

fn edges_by_column_pair(expanded: &ExpandedGraph) -> Vec<Vec<(LayoutVertexId, LayoutVertexId)>> {
    let columns_by_vertex: HashMap<_, _> = expanded
        .columns
        .iter()
        .enumerate()
        .flat_map(|(column, vertices)| vertices.iter().map(move |id| (id.clone(), column)))
        .collect();
    let mut pairs = vec![Vec::new(); expanded.columns.len().saturating_sub(1)];
    for (from, to) in &expanded.edges {
        let (Some(&from_column), Some(&to_column)) =
            (columns_by_vertex.get(from), columns_by_vertex.get(to))
        else {
            continue;
        };
        if from_column + 1 == to_column {
            pairs[from_column].push((from.clone(), to.clone()));
        }
    }
    for edges in &mut pairs {
        edges.sort_by_key(|(left, right)| (left.stable_key(), right.stable_key()));
        edges.dedup();
    }
    pairs
}

fn pair_crossing_count(
    edges: &[(LayoutVertexId, LayoutVertexId)],
    positions: &HashMap<LayoutVertexId, usize>,
) -> usize {
    let mut coordinates: Vec<_> = edges
        .iter()
        .filter_map(|(left, right)| Some((*positions.get(left)?, *positions.get(right)?)))
        .collect();
    coordinates.sort_unstable();
    let right_size = coordinates
        .iter()
        .map(|(_, right)| *right)
        .max()
        .map_or(0, |position| position + 2);
    let mut fenwick = vec![0_usize; right_size + 1];

    fn add(fenwick: &mut [usize], position: usize) {
        let mut index = position + 1;
        while index < fenwick.len() {
            fenwick[index] += 1;
            index += 1_usize << index.trailing_zeros();
        }
    }

    fn prefix_sum(fenwick: &[usize], position: usize) -> usize {
        let mut index = position + 1;
        let mut sum = 0;
        while index > 0 {
            sum += fenwick[index];
            index &= index - 1;
        }
        sum
    }

    let mut crossings = 0;
    let mut seen = 0;
    let mut group_start = 0;
    while group_start < coordinates.len() {
        let left_position = coordinates[group_start].0;
        let mut group_end = group_start + 1;
        while group_end < coordinates.len() && coordinates[group_end].0 == left_position {
            group_end += 1;
        }
        for &(_, right_position) in &coordinates[group_start..group_end] {
            crossings += seen - prefix_sum(&fenwick, right_position);
        }
        for &(_, right_position) in &coordinates[group_start..group_end] {
            add(&mut fenwick, right_position);
            seen += 1;
        }
        group_start = group_end;
    }
    crossings
}

fn refine_crossings(bands: &[BandId], expanded: &mut ExpandedGraph) {
    let pair_edges = edges_by_column_pair(expanded);
    let mut positions: HashMap<_, _> = expanded
        .columns
        .iter()
        .flat_map(|column| {
            column
                .iter()
                .enumerate()
                .map(|(position, id)| (id.clone(), position))
        })
        .collect();
    let mut pair_crossings: Vec<_> = pair_edges
        .iter()
        .map(|edges| pair_crossing_count(edges, &positions))
        .collect();

    for sweep in 0..SWAP_REFINEMENT_SWEEPS {
        let mut improved = false;
        let mut columns: Vec<_> = (0..expanded.columns.len()).collect();
        if sweep % 2 == 1 {
            columns.reverse();
        }
        for column_idx in columns {
            let mut sweep_bands: Vec<_> = bands.iter().collect();
            if sweep % 2 == 1 {
                sweep_bands.reverse();
            }
            for band in sweep_bands {
                let indices: Vec<_> = expanded.columns[column_idx]
                    .iter()
                    .enumerate()
                    .filter_map(|(index, id)| {
                        (expanded.bands.get(id) == Some(band)).then_some(index)
                    })
                    .collect();
                let mut pairs: Vec<_> = indices.windows(2).map(|pair| (pair[0], pair[1])).collect();
                if sweep % 2 == 1 {
                    pairs.reverse();
                }
                for (left, right) in pairs {
                    let mut affected_pairs = Vec::with_capacity(2);
                    if column_idx > 0 {
                        affected_pairs.push(column_idx - 1);
                    }
                    if column_idx < pair_edges.len() {
                        affected_pairs.push(column_idx);
                    }
                    let before: usize = affected_pairs
                        .iter()
                        .map(|pair| pair_crossings[*pair])
                        .sum();
                    let left_id = expanded.columns[column_idx][left].clone();
                    let right_id = expanded.columns[column_idx][right].clone();
                    expanded.columns[column_idx].swap(left, right);
                    positions.insert(left_id.clone(), right);
                    positions.insert(right_id.clone(), left);
                    let updated: Vec<_> = affected_pairs
                        .iter()
                        .map(|pair| pair_crossing_count(&pair_edges[*pair], &positions))
                        .collect();
                    let after: usize = updated.iter().sum();
                    if after < before {
                        improved = true;
                        for (pair, crossings) in affected_pairs.iter().zip(updated) {
                            pair_crossings[*pair] = crossings;
                        }
                    } else {
                        expanded.columns[column_idx].swap(left, right);
                        positions.insert(left_id, left);
                        positions.insert(right_id, right);
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
}

fn relax_coordinates(
    expanded: &ExpandedGraph,
    band_bounds: &[BandBounds],
    vertices: &mut [PositionedVertex],
) {
    let mut neighbors: HashMap<LayoutVertexId, Vec<LayoutVertexId>> = HashMap::new();
    for (from, to) in &expanded.edges {
        neighbors.entry(from.clone()).or_default().push(to.clone());
        neighbors.entry(to.clone()).or_default().push(from.clone());
    }
    for adjacent in neighbors.values_mut() {
        adjacent.sort_by_key(LayoutVertexId::stable_key);
        adjacent.dedup();
    }
    let vertex_index: HashMap<_, _> = vertices
        .iter()
        .enumerate()
        .map(|(index, vertex)| (vertex.id.clone(), index))
        .collect();
    let bounds_by_band: HashMap<_, _> = band_bounds
        .iter()
        .map(|bounds| (bounds.id.clone(), bounds))
        .collect();

    for _ in 0..RELAXATION_ITERATIONS {
        let prior: HashMap<_, _> = vertices
            .iter()
            .map(|vertex| (vertex.id.clone(), vertex.center_y))
            .collect();
        let mut desired = HashMap::new();
        for vertex in vertices.iter() {
            let Some(adjacent) = neighbors.get(&vertex.id) else {
                desired.insert(vertex.id.clone(), vertex.center_y);
                continue;
            };
            let mut positions = Vec::new();
            for neighbor in adjacent {
                if let Some(&center) = prior.get(neighbor) {
                    let weight = match neighbor {
                        LayoutVertexId::Real(_) => 2,
                        LayoutVertexId::Dummy { .. } => 1,
                    };
                    positions.extend(std::iter::repeat_n(center, weight));
                }
            }
            positions.sort_by(f32::total_cmp);
            let target = positions
                .get(positions.len() / 2)
                .copied()
                .unwrap_or(vertex.center_y);
            desired.insert(
                vertex.id.clone(),
                vertex.center_y
                    + (target - vertex.center_y).clamp(-MAX_RELAXATION_SHIFT, MAX_RELAXATION_SHIFT),
            );
        }

        for (column_idx, column) in expanded.columns.iter().enumerate() {
            for bounds in band_bounds {
                let ids: Vec<_> = column
                    .iter()
                    .filter(|id| expanded.bands.get(*id) == Some(&bounds.id))
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                project_centers(
                    &ids,
                    &desired,
                    &vertex_index,
                    bounds_by_band[&bounds.id],
                    expanded,
                    vertices,
                );
                debug_assert!(
                    ids.iter()
                        .all(|id| { vertices[vertex_index[*id]].column == column_idx })
                );
            }
        }
    }
}

fn project_centers(
    ids: &[&LayoutVertexId],
    desired: &HashMap<LayoutVertexId, f32>,
    vertex_index: &HashMap<LayoutVertexId, usize>,
    bounds: &BandBounds,
    expanded: &ExpandedGraph,
    vertices: &mut [PositionedVertex],
) {
    let mut centers: Vec<_> = ids
        .iter()
        .map(|id| {
            let vertex = &vertices[vertex_index[*id]];
            desired[id].clamp(
                bounds.content_top + vertex.height / 2.0,
                bounds.content_bottom - vertex.height / 2.0,
            )
        })
        .collect();

    for index in 1..centers.len() {
        let previous = &vertices[vertex_index[ids[index - 1]]];
        let current = &vertices[vertex_index[ids[index]]];
        let gap = vertex_gap(expanded, ids[index - 1], ids[index]);
        let minimum = centers[index - 1] + previous.height / 2.0 + gap + current.height / 2.0;
        centers[index] = centers[index].max(minimum);
    }
    if let Some(last) = centers.len().checked_sub(1) {
        let last_vertex = &vertices[vertex_index[ids[last]]];
        centers[last] = centers[last].min(bounds.content_bottom - last_vertex.height / 2.0);
        for index in (0..last).rev() {
            let current = &vertices[vertex_index[ids[index]]];
            let next = &vertices[vertex_index[ids[index + 1]]];
            let gap = vertex_gap(expanded, ids[index], ids[index + 1]);
            let maximum = centers[index + 1] - next.height / 2.0 - gap - current.height / 2.0;
            centers[index] = centers[index].min(maximum);
        }
        let first_vertex = &vertices[vertex_index[ids[0]]];
        centers[0] = centers[0].max(bounds.content_top + first_vertex.height / 2.0);
        for index in 1..centers.len() {
            let previous = &vertices[vertex_index[ids[index - 1]]];
            let current = &vertices[vertex_index[ids[index]]];
            let gap = vertex_gap(expanded, ids[index - 1], ids[index]);
            let minimum = centers[index - 1] + previous.height / 2.0 + gap + current.height / 2.0;
            centers[index] = centers[index].max(minimum);
        }
    }

    for (id, center) in ids.iter().zip(centers) {
        vertices[vertex_index[*id]].center_y = center;
    }
}

fn build_edge_routes(
    expanded: &ExpandedGraph,
    nodes: &[NodeRect],
    vertex_positions: &HashMap<LayoutVertexId, f32>,
    column_x: &[f32],
    column_widths: &[f32],
) -> Vec<EdgeRoute> {
    let node_rects: HashMap<_, _> = nodes.iter().map(|node| (node.id.clone(), node)).collect();
    let gate_offset = COLUMN_GAP * 0.18;
    let mut routes: Vec<_> = expanded
        .routes
        .iter()
        .filter_map(|route| {
            let (from_rect, to_rect) = (node_rects.get(&route.from)?, node_rects.get(&route.to)?);
            if route.is_reference
                && (from_rect.column == to_rect.column || from_rect.column == to_rect.column + 1)
            {
                return Some(EdgeRoute {
                    from: route.from.clone(),
                    to: route.to.clone(),
                    points: short_reference_route(from_rect, to_rect),
                });
            }
            let rightward = from_rect.column < to_rect.column;
            let mut points = vec![Point {
                x: if rightward {
                    from_rect.x + from_rect.width
                } else {
                    from_rect.x
                },
                y: from_rect.y + from_rect.height / 2.0,
            }];
            points.push(Point {
                x: if rightward {
                    column_x[from_rect.column] + column_widths[from_rect.column] + gate_offset
                } else {
                    column_x[from_rect.column] - gate_offset
                },
                y: points[0].y,
            });
            for vertex in route
                .vertices
                .iter()
                .skip(1)
                .take(route.vertices.len().saturating_sub(2))
            {
                let LayoutVertexId::Dummy { column, .. } = vertex else {
                    continue;
                };
                let &y = vertex_positions.get(vertex)?;
                points.push(Point {
                    x: if rightward {
                        column_x[*column] - gate_offset
                    } else {
                        column_x[*column] + column_widths[*column] + gate_offset
                    },
                    y,
                });
                points.push(Point {
                    x: if rightward {
                        column_x[*column] + column_widths[*column] + gate_offset
                    } else {
                        column_x[*column] - gate_offset
                    },
                    y,
                });
            }
            points.push(Point {
                x: if rightward {
                    column_x[to_rect.column] - gate_offset
                } else {
                    column_x[to_rect.column] + column_widths[to_rect.column] + gate_offset
                },
                y: to_rect.y + to_rect.height / 2.0,
            });
            points.push(Point {
                x: if rightward {
                    to_rect.x
                } else {
                    to_rect.x + to_rect.width
                },
                y: to_rect.y + to_rect.height / 2.0,
            });
            points.dedup();
            Some(EdgeRoute {
                from: route.from.clone(),
                to: route.to.clone(),
                points,
            })
        })
        .collect();
    spread_card_ports(&mut routes, &node_rects);
    routes
}

fn spread_card_ports(routes: &mut [EdgeRoute], node_rects: &HashMap<GraphNodeId, &NodeRect>) {
    let mut groups: HashMap<(GraphNodeId, CardSide), Vec<PortEndpoint>> = HashMap::new();

    for (route_index, route) in routes.iter().enumerate() {
        let (Some(from_rect), Some(to_rect), Some(start), Some(end)) = (
            node_rects.get(&route.from),
            node_rects.get(&route.to),
            route.points.first(),
            route.points.last(),
        ) else {
            continue;
        };
        let endpoints = [
            (
                route.from.clone(),
                card_side(from_rect, *start),
                RouteEndpoint::Start,
                *to_rect,
                route.to.stable_key(),
            ),
            (
                route.to.clone(),
                card_side(to_rect, *end),
                RouteEndpoint::End,
                *from_rect,
                route.from.stable_key(),
            ),
        ];
        for (card, side, endpoint, other, other_key) in endpoints {
            let Some(side) = side else {
                continue;
            };
            let (other_primary, other_secondary) = match side {
                CardSide::Left | CardSide::Right => {
                    (other.y + other.height / 2.0, other.x + other.width / 2.0)
                }
                CardSide::Top | CardSide::Bottom => {
                    (other.x + other.width / 2.0, other.y + other.height / 2.0)
                }
            };
            groups.entry((card, side)).or_default().push(PortEndpoint {
                route_index,
                endpoint,
                other_primary,
                other_secondary,
                stable_key: other_key,
            });
        }
    }

    for ((card, side), endpoints) in &mut groups {
        let Some(rect) = node_rects.get(card) else {
            continue;
        };
        endpoints.sort_by(|left, right| {
            left.other_primary
                .total_cmp(&right.other_primary)
                .then_with(|| left.other_secondary.total_cmp(&right.other_secondary))
                .then_with(|| left.stable_key.cmp(&right.stable_key))
                .then_with(|| left.route_index.cmp(&right.route_index))
        });

        let (minimum, maximum) = match side {
            CardSide::Left | CardSide::Right => (
                rect.y + rect.height * PORT_MARGIN_FRACTION,
                rect.y + rect.height * (1.0 - PORT_MARGIN_FRACTION),
            ),
            CardSide::Top | CardSide::Bottom => (
                rect.x + rect.width * PORT_MARGIN_FRACTION,
                rect.x + rect.width * (1.0 - PORT_MARGIN_FRACTION),
            ),
        };
        let count = endpoints.len();
        for (index, endpoint) in endpoints.iter().enumerate() {
            let coordinate = if count == 1 {
                (minimum + maximum) / 2.0
            } else {
                minimum + (maximum - minimum) * index as f32 / (count - 1) as f32
            };
            set_route_port(
                &mut routes[endpoint.route_index],
                endpoint.endpoint,
                *side,
                coordinate,
            );
        }
    }

    for route in routes {
        expand_shifted_direct_route(route, node_rects);
        route.points.dedup();
    }
}

fn card_side(rect: &NodeRect, point: Point) -> Option<CardSide> {
    if point.x == rect.x {
        Some(CardSide::Left)
    } else if point.x == rect.x + rect.width {
        Some(CardSide::Right)
    } else if point.y == rect.y {
        Some(CardSide::Top)
    } else if point.y == rect.y + rect.height {
        Some(CardSide::Bottom)
    } else {
        None
    }
}

fn set_route_port(route: &mut EdgeRoute, endpoint: RouteEndpoint, side: CardSide, coordinate: f32) {
    let endpoint_index = match endpoint {
        RouteEndpoint::Start => 0,
        RouteEndpoint::End => route.points.len() - 1,
    };
    match side {
        CardSide::Left | CardSide::Right => route.points[endpoint_index].y = coordinate,
        CardSide::Top | CardSide::Bottom => route.points[endpoint_index].x = coordinate,
    }

    if route.points.len() < 4 {
        return;
    }
    let gate_index = match endpoint {
        RouteEndpoint::Start => 1,
        RouteEndpoint::End => route.points.len() - 2,
    };
    match side {
        CardSide::Left | CardSide::Right => route.points[gate_index].y = coordinate,
        CardSide::Top | CardSide::Bottom => route.points[gate_index].x = coordinate,
    }
}

fn expand_shifted_direct_route(
    route: &mut EdgeRoute,
    node_rects: &HashMap<GraphNodeId, &NodeRect>,
) {
    if route.points.len() != 2 {
        return;
    }
    let (Some(from_rect), Some(to_rect)) = (node_rects.get(&route.from), node_rects.get(&route.to))
    else {
        return;
    };
    let start = route.points[0];
    let end = route.points[1];
    let (Some(start_side), Some(end_side)) = (card_side(from_rect, start), card_side(to_rect, end))
    else {
        return;
    };
    if matches!(start_side, CardSide::Top | CardSide::Bottom)
        && matches!(end_side, CardSide::Top | CardSide::Bottom)
        && start.x != end.x
    {
        let corridor_y = (start.y + end.y) / 2.0;
        route.points = vec![
            start,
            Point {
                x: start.x,
                y: corridor_y,
            },
            Point {
                x: end.x,
                y: corridor_y,
            },
            end,
        ];
    }
}

fn short_reference_route(from: &NodeRect, to: &NodeRect) -> Vec<Point> {
    let from_top = from.y;
    let from_bottom = from.y + from.height;
    let to_top = to.y;
    let to_bottom = to.y + to.height;
    let from_x = from.x + from.width / 2.0;
    let to_x = to.x + to.width / 2.0;

    let (start, end, corridor_y) = if from_bottom <= to_top {
        (
            Point {
                x: from_x,
                y: from_bottom,
            },
            Point { x: to_x, y: to_top },
            (from_bottom + to_top) / 2.0,
        )
    } else if to_bottom <= from_top {
        (
            Point {
                x: from_x,
                y: from_top,
            },
            Point {
                x: to_x,
                y: to_bottom,
            },
            (to_bottom + from_top) / 2.0,
        )
    } else {
        let offset = NODE_GAP / 2.0;
        let top_corridor = from_top.min(to_top) - offset;
        let bottom_corridor = from_bottom.max(to_bottom) + offset;
        let top_cost = from_top - top_corridor + to_top - top_corridor;
        let bottom_cost = bottom_corridor - from_bottom + bottom_corridor - to_bottom;
        if top_cost <= bottom_cost {
            (
                Point {
                    x: from_x,
                    y: from_top,
                },
                Point { x: to_x, y: to_top },
                top_corridor,
            )
        } else {
            (
                Point {
                    x: from_x,
                    y: from_bottom,
                },
                Point {
                    x: to_x,
                    y: to_bottom,
                },
                bottom_corridor,
            )
        }
    };

    let mut points = vec![start];
    if from_x != to_x {
        points.push(Point {
            x: from_x,
            y: corridor_y,
        });
        points.push(Point {
            x: to_x,
            y: corridor_y,
        });
    }
    points.push(end);
    points.dedup();
    points
}

pub(crate) fn edge_segment_controls(from: Point, to: Point) -> (Point, Point) {
    let middle_x = (from.x + to.x) / 2.0;
    (
        Point {
            x: middle_x,
            y: from.y,
        },
        Point {
            x: middle_x,
            y: to.y,
        },
    )
}

#[cfg(test)]
fn crossing_counts_before_and_after_refinement(model: &GraphModel) -> (usize, usize) {
    let layering = graph_layering(model);
    let bands = ordered_bands(model);
    let band_rank: HashMap<_, _> = bands
        .iter()
        .cloned()
        .enumerate()
        .map(|(rank, band)| (band, rank))
        .collect();
    let mut expanded = expand_long_edges(model, &layering);
    sort_initial_columns(model, &band_rank, &mut expanded);
    reduce_crossings(model, &bands, &mut expanded);
    let before = crossing_count(&expanded.columns, &expanded.edges);
    refine_crossings(&bands, &mut expanded);
    (before, crossing_count(&expanded.columns, &expanded.edges))
}

#[cfg(test)]
fn edge_route_intersects_rect(route: &EdgeRoute, rect: &NodeRect) -> bool {
    route.points.windows(2).any(|segment| {
        let from = segment[0];
        let to = segment[1];
        let (control_one, control_two) = edge_segment_controls(from, to);
        (0..=64).any(|step| {
            let t = step as f32 / 64.0;
            let one_minus_t = 1.0 - t;
            let point = Point {
                x: one_minus_t.powi(3) * from.x
                    + 3.0 * one_minus_t.powi(2) * t * control_one.x
                    + 3.0 * one_minus_t * t.powi(2) * control_two.x
                    + t.powi(3) * to.x,
                y: one_minus_t.powi(3) * from.y
                    + 3.0 * one_minus_t.powi(2) * t * control_one.y
                    + 3.0 * one_minus_t * t.powi(2) * control_two.y
                    + t.powi(3) * to.y,
            };
            point.x > rect.x
                && point.x < rect.x + rect.width
                && point.y > rect.y
                && point.y < rect.y + rect.height
        })
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use carina_core::effect::{Effect, EffectGeneration};
    use carina_core::executor::{PlanSchedulerPhase, UnresolvedResource};
    use carina_core::plan::Plan;
    use carina_core::provider::AppliedIdentifier;
    use carina_core::resource::{
        CallSite, ExpansionTrace, PersistentId, ResolvedResource, ResolvedResourceId, Resource,
        ResourceId, State, Value,
    };

    use super::*;
    use crate::plan_image::model::{PlanImageInput, build_model};

    fn resource(binding: &str, dependencies: &[&str]) -> ResolvedResource {
        let identity = carina_core::resource::ResourceIdentity::try_from(binding.to_string())
            .expect("fixture identity");
        let mut resource = Resource::new("test.Widget", identity);
        resource.binding = Some(binding.to_string());
        for dependency in dependencies {
            resource.set_attr(
                format!("ref_{dependency}"),
                Value::resource_ref((*dependency).to_string(), "id".to_string(), vec![]),
            );
        }
        let dependencies = dependencies
            .iter()
            .map(|dependency| {
                let identity =
                    carina_core::resource::ResourceIdentity::try_from((*dependency).to_string())
                        .expect("fixture dependency identity");
                ResolvedResourceId::new(ResourceId::with_identity("test.Widget", identity))
            })
            .collect();
        let resource = ResolvedResource::collect_resolved([resource])
            .expect("fixture identity resolves")
            .remove(0)
            .into_inner();
        ResolvedResource::from_resolved_parts(resource, dependencies, Default::default())
    }

    fn model_for(
        plan: &Plan,
        desired: &[ResolvedResource],
        trace: Option<&ExpansionTrace>,
    ) -> GraphModel {
        let unresolved: Vec<_> = desired
            .iter()
            .map(|resource| UnresolvedResource::from_resolved_for_test(resource.as_inner().clone()))
            .collect();
        let summary = crate::plan_summary::PlanChangeSummary::new(
            plan,
            &[],
            &crate::plan_summary::resolved_deferred_for_for_test([]),
            &[],
        );
        build_model(PlanImageInput {
            plan,
            desired_resources: desired,
            unresolved_resources: &unresolved,
            compositions: &[],
            expansion_trace: trace,
            state: None,
            summary: &summary,
        })
    }

    fn fixture_model(name: &str) -> GraphModel {
        let fixture = crate::fixture_plan::build_plan_from_fixture_name(name);
        let summary = crate::fixture_plan::build_fixture_plan_change_summary(&fixture);
        build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &fixture.desired_resources,
            unresolved_resources: &fixture.unresolved_resources,
            compositions: &fixture.compositions,
            expansion_trace: Some(&fixture.expansion_trace),
            state: None,
            summary: &summary,
        })
    }

    fn positioned_vertices_for_test(
        model: &GraphModel,
    ) -> (ExpandedGraph, Vec<PositionedVertex>, Vec<BandBounds>) {
        let layering = graph_layering(model);
        let bands = ordered_bands(model);
        let band_rank: HashMap<_, _> = bands
            .iter()
            .cloned()
            .enumerate()
            .map(|(rank, band)| (band, rank))
            .collect();
        let mut expanded = expand_long_edges(model, &layering);
        sort_initial_columns(model, &band_rank, &mut expanded);
        reduce_crossings(model, &bands, &mut expanded);
        refine_crossings(&bands, &mut expanded);

        let band_heights: Vec<_> = bands
            .iter()
            .map(|band| {
                let max_stack = expanded
                    .columns
                    .iter()
                    .map(|column| stack_height(model, &expanded, column, band))
                    .fold(0.0_f32, f32::max);
                (band_label_height(band) + max_stack + BAND_PADDING * 2.0).max(58.0)
            })
            .collect();
        let mut cursor_y = HEADER_HEIGHT;
        let band_tops: Vec<_> = band_heights
            .iter()
            .map(|height| {
                let top = cursor_y;
                cursor_y += *height + BAND_GAP;
                top
            })
            .collect();
        let band_bounds: Vec<_> = bands
            .iter()
            .enumerate()
            .map(|(index, band)| BandBounds {
                id: band.clone(),
                content_top: band_tops[index] + BAND_PADDING + band_label_height(band),
                content_bottom: band_tops[index] + band_heights[index] - BAND_PADDING,
            })
            .collect();
        let mut vertices = Vec::new();
        for (column_idx, column) in expanded.columns.iter().enumerate() {
            for (band_idx, band) in bands.iter().enumerate() {
                let band_vertices: Vec<_> = column
                    .iter()
                    .filter(|id| expanded.bands.get(*id) == Some(band))
                    .collect();
                let stack = stack_height(model, &expanded, column, band);
                let bounds = &band_bounds[band_idx];
                let mut y = bounds.content_top
                    + ((bounds.content_bottom - bounds.content_top) - stack).max(0.0) / 2.0;
                for (index, id) in band_vertices.iter().enumerate() {
                    let height = vertex_height(model, id);
                    vertices.push(PositionedVertex {
                        id: (*id).clone(),
                        column: column_idx,
                        center_y: y + height / 2.0,
                        height,
                    });
                    y += height;
                    if let Some(next) = band_vertices.get(index + 1) {
                        y += vertex_gap(&expanded, id, next);
                    }
                }
            }
        }
        relax_coordinates(&expanded, &band_bounds, &mut vertices);
        (expanded, vertices, band_bounds)
    }

    fn update(resource: ResolvedResource) -> Effect {
        Effect::Update {
            from: Box::new(State::existing(resource.id.clone(), HashMap::new())),
            identifier: AppliedIdentifier::new(format!(
                "{}-id",
                resource.binding.as_deref().expect("test resource is bound")
            ))
            .unwrap(),
            to: resource,
            changed_attributes: vec!["description".to_string()],
        }
    }

    fn aggregated_effect_node(plan: &Plan, effect_idx: usize) -> GraphNodeId {
        let replacement_deletes: HashMap<_, _> = plan
            .replace_display_info()
            .map(|replacement| (replacement.delete_idx, replacement.create_idx))
            .collect();
        GraphNodeId::Effect(
            replacement_deletes
                .get(&effect_idx)
                .copied()
                .unwrap_or(effect_idx),
        )
    }

    fn assert_ordering_edges_match_executor(fixture_name: &str) {
        let fixture = crate::fixture_plan::build_plan_from_fixture_name(fixture_name);
        if fixture_name == "deferred_for_with_paired_destroy" {
            assert!(
                fixture
                    .plan
                    .effects()
                    .iter()
                    .any(|effect| matches!(effect, Effect::DeferredReplace(_))),
                "fixture must exercise DeferredReplace scheduler expansion"
            );
        }
        let summary = crate::fixture_plan::build_fixture_plan_change_summary(&fixture);
        let model = build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &fixture.desired_resources,
            unresolved_resources: &fixture.unresolved_resources,
            compositions: &fixture.compositions,
            expansion_trace: Some(&fixture.expansion_trace),
            state: None,
            summary: &summary,
        });
        let unresolved: HashMap<_, _> = fixture
            .desired_resources
            .iter()
            .zip(&fixture.unresolved_resources)
            .map(|(resolved, unresolved)| (resolved.resolved_id(), unresolved))
            .collect();
        let replacement_delete_indices: HashSet<_> = fixture
            .plan
            .replace_display_info()
            .map(|replacement| replacement.delete_idx)
            .collect();
        let scheduler_edges = carina_core::executor::build_plan_scheduler_dependency_edges(
            fixture.plan.effects(),
            &unresolved,
        );
        if fixture_name == "deferred_for_with_paired_destroy" {
            assert!(scheduler_edges.iter().any(|(from, to)| {
                from.phase == PlanSchedulerPhase::DeferredReplaceTeardown
                    || to.phase == PlanSchedulerPhase::DeferredReplaceTeardown
            }));
        }
        let expected: HashSet<_> = scheduler_edges
            .into_iter()
            .filter_map(|(from, to)| {
                if from.phase == PlanSchedulerPhase::DeferredReplaceTeardown
                    || to.phase == PlanSchedulerPhase::DeferredReplaceTeardown
                    || replacement_delete_indices.contains(&from.effect_index)
                    || replacement_delete_indices.contains(&to.effect_index)
                {
                    return None;
                }
                let from = aggregated_effect_node(&fixture.plan, from.effect_index);
                let to = aggregated_effect_node(&fixture.plan, to.effect_index);
                (from != to
                    && model
                        .node(&from)
                        .is_some_and(|node| node.kind != EffectKind::Unchanged)
                    && model
                        .node(&to)
                        .is_some_and(|node| node.kind != EffectKind::Unchanged))
                .then_some((from, to))
            })
            .collect();
        let actual: HashSet<_> = model
            .ordering_edges
            .iter()
            .map(|edge| (edge.from.clone(), edge.to.clone()))
            .collect();

        assert_eq!(actual, expected, "fixture {fixture_name}");

        let layout = layout_graph(&model);
        let reversed_edges = cycle_removed_ordering_edges(&model).1;
        assert!(
            reversed_edges.is_empty(),
            "filtered executor graph must be acyclic for {fixture_name}: {:?}",
            reversed_edges
        );
        for (from, to) in expected {
            let from_rect = layout.node(&from).expect("visible executor edge source");
            let to_rect = layout.node(&to).expect("visible executor edge target");
            assert!(
                from_rect.column < to_rect.column,
                "executor edge must advance a column in {fixture_name}: {from:?} -> {to:?}"
            );
        }

        if fixture_name == "plan_image_module_cascade" {
            let source = model
                .nodes
                .iter()
                .find(|node| node.label == "source")
                .expect("source replacement");
            let cascade = model
                .nodes
                .iter()
                .find(|node| node.label == "cascade")
                .expect("cascade replacement");
            let source_column = layout.node(&source.id).expect("source card").column;
            let cascade_column = layout.node(&cascade.id).expect("cascade card").column;
            assert_eq!(source_column, 0, "root replacement starts the cascade");
            assert!(source_column < cascade_column, "cascade follows source");
        }
    }

    #[test]
    fn dependency_is_always_in_an_earlier_column_than_dependent() {
        let dependency = resource("database", &[]);
        let dependent = resource("service", &["database"]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(dependency.clone()));
        plan.add(Effect::Create(dependent.clone()));
        let desired = vec![dependency, dependent];
        let layout = layout_graph(&model_for(&plan, &desired, None));

        assert!(
            layout.node(&GraphNodeId::Effect(0)).unwrap().column
                < layout.node(&GraphNodeId::Effect(1)).unwrap().column
        );
    }

    #[test]
    fn shared_vertical_card_sides_spread_ports_in_other_endpoint_order() {
        let source = resource("source", &[]);
        let upper = resource("upper", &["source"]);
        let middle = resource("middle", &["source"]);
        let lower = resource("lower", &["source"]);
        let sink = resource("sink", &["upper", "middle", "lower"]);
        let desired = vec![
            source.clone(),
            upper.clone(),
            middle.clone(),
            lower.clone(),
            sink.clone(),
        ];
        let mut plan = Plan::new();
        for resource in [source, upper, middle, lower, sink] {
            plan.add(Effect::Create(resource));
        }

        let layout = layout_graph(&model_for(&plan, &desired, None));
        let source_rect = layout.node(&GraphNodeId::Effect(0)).expect("source card");
        let sink_rect = layout.node(&GraphNodeId::Effect(4)).expect("sink card");

        let mut outgoing_ports = (1..=3)
            .map(|target_index| {
                let target = layout
                    .node(&GraphNodeId::Effect(target_index))
                    .expect("middle-layer card");
                let route = layout
                    .edge_route(&GraphNodeId::Effect(0), &GraphNodeId::Effect(target_index))
                    .expect("source fan-out route");
                let port = route.points.first().expect("route start");
                assert_eq!(port.x, source_rect.x + source_rect.width);
                (target.y + target.height / 2.0, port.y)
            })
            .collect::<Vec<_>>();
        outgoing_ports.sort_by(|left, right| left.0.total_cmp(&right.0));
        assert!(
            outgoing_ports
                .windows(2)
                .all(|ports| ports[0].1 < ports[1].1),
            "fan-out ports must be distinct and follow target y: {outgoing_ports:?}"
        );

        let mut incoming_ports = (1..=3)
            .map(|source_index| {
                let source = layout
                    .node(&GraphNodeId::Effect(source_index))
                    .expect("middle-layer card");
                let route = layout
                    .edge_route(&GraphNodeId::Effect(source_index), &GraphNodeId::Effect(4))
                    .expect("sink fan-in route");
                let port = route.points.last().expect("route end");
                assert_eq!(port.x, sink_rect.x);
                (source.y + source.height / 2.0, port.y)
            })
            .collect::<Vec<_>>();
        incoming_ports.sort_by(|left, right| left.0.total_cmp(&right.0));
        assert!(
            incoming_ports
                .windows(2)
                .all(|ports| ports[0].1 < ports[1].1),
            "fan-in ports must be distinct and follow source y: {incoming_ports:?}"
        );
    }

    #[test]
    fn shared_horizontal_card_side_spreads_incoming_and_outgoing_ports_together() {
        let cards = [
            NodeRect {
                id: GraphNodeId::Effect(0),
                x: 100.0,
                y: 100.0,
                width: 100.0,
                height: 80.0,
                column: 1,
            },
            NodeRect {
                id: GraphNodeId::Effect(1),
                x: 0.0,
                y: 0.0,
                width: 20.0,
                height: 20.0,
                column: 0,
            },
            NodeRect {
                id: GraphNodeId::Effect(2),
                x: 140.0,
                y: 0.0,
                width: 20.0,
                height: 20.0,
                column: 1,
            },
            NodeRect {
                id: GraphNodeId::Effect(3),
                x: 200.0,
                y: 0.0,
                width: 20.0,
                height: 20.0,
                column: 2,
            },
            NodeRect {
                id: GraphNodeId::Effect(4),
                x: 140.0,
                y: 260.0,
                width: 20.0,
                height: 20.0,
                column: 1,
            },
        ];
        let node_rects = cards
            .iter()
            .map(|card| (card.id.clone(), card))
            .collect::<HashMap<_, _>>();
        let mut routes = vec![
            EdgeRoute {
                from: GraphNodeId::Effect(1),
                to: GraphNodeId::Effect(0),
                points: vec![Point { x: 10.0, y: 20.0 }, Point { x: 150.0, y: 100.0 }],
            },
            EdgeRoute {
                from: GraphNodeId::Effect(2),
                to: GraphNodeId::Effect(0),
                points: vec![Point { x: 150.0, y: 20.0 }, Point { x: 150.0, y: 100.0 }],
            },
            EdgeRoute {
                from: GraphNodeId::Effect(0),
                to: GraphNodeId::Effect(3),
                points: vec![Point { x: 150.0, y: 100.0 }, Point { x: 210.0, y: 20.0 }],
            },
            EdgeRoute {
                from: GraphNodeId::Effect(0),
                to: GraphNodeId::Effect(4),
                points: vec![Point { x: 150.0, y: 180.0 }, Point { x: 150.0, y: 260.0 }],
            },
        ];

        spread_card_ports(&mut routes, &node_rects);

        let mut top_ports = vec![
            routes[0].points.last().unwrap().x,
            routes[1].points.last().unwrap().x,
            routes[2].points.first().unwrap().x,
        ];
        top_ports.sort_by(f32::total_cmp);
        assert_eq!(top_ports, [120.0, 150.0, 180.0]);
        assert_eq!(
            routes[3].points.first().unwrap().x,
            150.0,
            "a side with one edge keeps its center port"
        );
    }

    #[test]
    fn reference_layout_draws_changed_to_changed_reference_without_ordering_edge() {
        let model = fixture_model("plan_image_reference_layout");
        let subnet = model
            .nodes
            .iter()
            .find(|node| node.label == "subnet")
            .expect("subnet node")
            .id
            .clone();
        let dns = model
            .nodes
            .iter()
            .find(|node| node.label == "dns")
            .expect("dns node")
            .id
            .clone();

        assert!(
            model
                .logical_edges
                .iter()
                .any(|edge| { edge.dependency == subnet && edge.dependent == dns })
        );
        assert!(!model.ordering_edges.iter().any(|edge| {
            (edge.from == subnet && edge.to == dns) || (edge.from == dns && edge.to == subnet)
        }));
        assert!(
            model
                .reference_edges
                .iter()
                .any(|edge| { edge.from == subnet && edge.to == dns })
        );
        assert!(layout_graph(&model).edge_route(&subnet, &dns).is_some());
    }

    #[test]
    fn reference_layout_drawn_edges_never_point_left_except_reversed_delete_ordering() {
        let model = fixture_model("plan_image_reference_layout");
        let layout = layout_graph(&model);
        let ordering_pairs: HashSet<_> = model
            .ordering_edges
            .iter()
            .map(|edge| (edge.from.clone(), edge.to.clone()))
            .collect();

        // Exact invariant: a rendered reference edge may advance or stay in its
        // column, and a rendered ordering edge must advance. The only permitted
        // left-pointing edge is an ordering edge between deletes whose direction
        // is the reverse of their logical dependency (dependent deletes first).
        for edge in model.ordering_edges.iter().chain(&model.reference_edges) {
            let Some(route) = layout.edge_route(&edge.from, &edge.to) else {
                continue;
            };
            assert!(!route.points.is_empty());
            let from = layout.node(&edge.from).expect("drawn edge source");
            let to = layout.node(&edge.to).expect("drawn edge target");
            let is_ordering = ordering_pairs.contains(&(edge.from.clone(), edge.to.clone()));
            let reversed_delete_ordering = is_ordering
                && model
                    .node(&edge.from)
                    .is_some_and(|node| node.kind == EffectKind::Delete)
                && model
                    .node(&edge.to)
                    .is_some_and(|node| node.kind == EffectKind::Delete)
                && model
                    .logical_edges
                    .iter()
                    .any(|logical| logical.dependency == edge.to && logical.dependent == edge.from);

            assert!(
                from.column <= to.column || reversed_delete_ordering,
                "edge points left without delete-order reversal: {:?} (column {}) -> {:?} (column {})",
                edge.from,
                from.column,
                edge.to,
                to.column
            );
            if is_ordering {
                assert_ne!(
                    from.column, to.column,
                    "cards in one column must have no ordering dependency"
                );
            }
        }

        let legacy = model
            .nodes
            .iter()
            .find(|node| node.label == "legacy")
            .expect("legacy delete")
            .id
            .clone();
        let worker = model
            .nodes
            .iter()
            .find(|node| node.label == "legacy_worker")
            .expect("legacy worker delete")
            .id
            .clone();
        assert!(
            model
                .logical_edges
                .iter()
                .any(|edge| { edge.dependency == legacy && edge.dependent == worker })
        );
        assert!(
            model
                .ordering_edges
                .iter()
                .any(|edge| { edge.from == worker && edge.to == legacy })
        );
        assert!(!model.reference_edges.iter().any(|edge| {
            (edge.from == legacy && edge.to == worker) || (edge.from == worker && edge.to == legacy)
        }));
    }

    #[test]
    fn reference_layout_haloed_module_cards_keep_four_pixel_border_margin() {
        const HALO_OUTSET: f32 = 5.0 + 8.0 / 2.0;
        const REQUIRED_MARGIN: f32 = 4.0;

        let model = fixture_model("plan_image_reference_layout");
        let layout = layout_graph(&model);
        for band in &layout.modules {
            for card in &layout.nodes {
                let Some(node) = model.node(&card.id) else {
                    continue;
                };
                if node.module.as_ref() != Some(&band.id) || node.affected_by.is_empty() {
                    continue;
                }
                let margins = [
                    card.x - HALO_OUTSET - band.x,
                    band.x + band.width - (card.x + card.width + HALO_OUTSET),
                    card.y - HALO_OUTSET - band.y,
                    band.y + band.height - (card.y + card.height + HALO_OUTSET),
                ];
                assert!(
                    margins.iter().all(|margin| *margin >= REQUIRED_MARGIN),
                    "halo around {:?} has band-border margins {margins:?}",
                    card.id
                );
            }
        }
    }

    #[test]
    fn reference_layout_module_band_hugs_members_without_overlapping_non_members() {
        let model = fixture_model("plan_image_reference_layout");
        let layout = layout_graph(&model);
        let overlaps = |left: (f32, f32, f32, f32), right: (f32, f32, f32, f32)| {
            left.0 < right.0 + right.2
                && right.0 < left.0 + left.2
                && left.1 < right.1 + right.3
                && right.1 < left.1 + left.3
        };

        for band in &layout.modules {
            let members: Vec<_> = layout
                .nodes
                .iter()
                .filter(|card| {
                    model
                        .node(&card.id)
                        .is_some_and(|node| node.module.as_ref() == Some(&band.id))
                })
                .collect();
            assert!(!members.is_empty());
            let member_left = members
                .iter()
                .map(|card| card.x)
                .fold(f32::INFINITY, f32::min);
            let member_right = members
                .iter()
                .map(|card| card.x + card.width)
                .fold(f32::NEG_INFINITY, f32::max);
            assert!((band.x - (member_left - BAND_PADDING)).abs() < 0.01);
            assert!(
                (band.x + band.width - (member_right + BAND_PADDING)).abs() < 0.01,
                "module band must end one padding width after its rightmost member"
            );

            for card in &layout.nodes {
                let is_member = model
                    .node(&card.id)
                    .is_some_and(|node| node.module.as_ref() == Some(&band.id));
                if !is_member {
                    assert!(
                        !overlaps(
                            (band.x, band.y, band.width, band.height),
                            (card.x, card.y, card.width, card.height)
                        ),
                        "module band {:?} overlaps non-member card {:?}",
                        band.id,
                        card.id
                    );
                }
            }
        }
        for (index, left) in layout.modules.iter().enumerate() {
            for right in &layout.modules[index + 1..] {
                assert!(!overlaps(
                    (left.x, left.y, left.width, left.height),
                    (right.x, right.y, right.width, right.height)
                ));
            }
        }
    }

    #[test]
    fn image_ordering_edges_equal_executor_edges_for_representative_plans() {
        for fixture in [
            "mixed_operations",
            "plan_image_module_cascade",
            "deferred_for_with_paired_destroy",
            "replace_create_only",
        ] {
            assert_ordering_edges_match_executor(fixture);
        }
    }

    #[test]
    fn unchanged_connector_does_not_serialize_independent_changed_effects() {
        let a = resource("a", &[]);
        let connector = resource("connector", &["a"]);
        let b = resource("b", &["connector"]);
        let connector_id = GraphNodeId::Unchanged(ResolvedResourceId::new(connector.id.clone()));
        let desired = vec![a.clone(), connector, b.clone()];
        let mut plan = Plan::new();
        plan.add(update(a));
        plan.add(update(b));

        let model = model_for(&plan, &desired, None);
        let layout = layout_graph(&model);

        assert!(model.ordering_edges.is_empty());
        assert_eq!(
            layout.node(&GraphNodeId::Effect(0)).unwrap().column,
            layout.node(&GraphNodeId::Effect(1)).unwrap().column,
            "the executor can run both updates in parallel"
        );
        assert_eq!(
            layout.node(&connector_id).unwrap().column,
            layout.node(&GraphNodeId::Effect(0)).unwrap().column,
            "a connector with no free column must stack without moving changed nodes"
        );
        assert_eq!(
            model
                .reference_edges
                .iter()
                .map(|edge| (edge.from.clone(), edge.to.clone()))
                .collect::<HashSet<_>>(),
            HashSet::from([
                (GraphNodeId::Effect(0), connector_id.clone()),
                (connector_id, GraphNodeId::Effect(1)),
            ])
        );

        for edge in &model.reference_edges {
            let from = layout.node(&edge.from).expect("reference source card");
            let to = layout.node(&edge.to).expect("reference target card");
            let route = layout
                .edge_route(&edge.from, &edge.to)
                .expect("reference route");
            assert_vertical_card_edge_route(route, from, to);
            let vertical_gap = if from.y < to.y {
                to.y - (from.y + from.height)
            } else {
                from.y - (to.y + to.height)
            };
            assert!(
                vertical_gap >= NODE_GAP * 3.0 - 0.01,
                "same-column reference cards need enough space to show dashes: {vertical_gap}"
            );
        }
    }

    #[test]
    fn backward_reference_uses_card_edge_connector_instead_of_an_outer_loop() {
        let a = resource("a", &["u"]);
        let b = resource("b", &["a"]);
        let u = resource("u", &["b"]);
        let u_id = GraphNodeId::Unchanged(ResolvedResourceId::new(u.id.clone()));
        let desired = vec![a.clone(), b.clone(), u];
        let mut plan = Plan::new();
        plan.add(Effect::Create(a));
        plan.add(Effect::Create(b));

        let model = model_for(&plan, &desired, None);
        let layout = layout_graph(&model);
        let edge = model
            .reference_edges
            .iter()
            .find(|edge| edge.from == u_id && edge.to == GraphNodeId::Effect(0))
            .expect("backward unchanged reference");
        let from = layout.node(&edge.from).expect("reference source card");
        let to = layout.node(&edge.to).expect("reference target card");
        assert!(from.column > to.column, "fixture must route backward");

        let route = layout
            .edge_route(&edge.from, &edge.to)
            .expect("backward reference route");
        assert_vertical_card_edge_route(route, from, to);
        let left = from.x.min(to.x);
        let right = (from.x + from.width).max(to.x + to.width);
        assert!(
            route
                .points
                .iter()
                .all(|point| point.x >= left && point.x <= right)
        );
    }

    #[test]
    fn long_backward_reference_uses_dummies_and_avoids_non_endpoint_cards() {
        let b = resource("b", &["u"]);
        let c = resource("c", &["b"]);
        let d = resource("d", &["c"]);
        let a = resource("a", &["d"]);
        let u = resource("u", &["a"]);
        let u_id = GraphNodeId::Unchanged(ResolvedResourceId::new(u.id.clone()));
        let desired = vec![b.clone(), c.clone(), d.clone(), a.clone(), u];
        let mut plan = Plan::new();
        for resource in [b, c, d, a] {
            plan.add(Effect::Create(resource));
        }

        let model = model_for(&plan, &desired, None);
        let layout = layout_graph(&model);
        let route = layout
            .edge_route(&u_id, &GraphNodeId::Effect(0))
            .expect("u -> b is the long backward reference");
        let from = layout.node(&u_id).expect("unchanged source card");
        let to = layout.node(&GraphNodeId::Effect(0)).expect("b card");

        assert_eq!(from.column, 3, "u stacks beside its dependency a");
        assert_eq!(to.column, 0);
        assert!(
            route.points.len() > 4,
            "a multi-column backward reference must route through dummies"
        );
        for card in &layout.nodes {
            if card.id != u_id && card.id != GraphNodeId::Effect(0) {
                assert!(
                    !edge_route_intersects_rect(route, card),
                    "backward reference intersects non-endpoint card {:?}",
                    card.id
                );
            }
        }
    }

    fn assert_vertical_card_edge_route(route: &EdgeRoute, from: &NodeRect, to: &NodeRect) {
        let first = route.points.first().expect("route start");
        let last = route.points.last().expect("route end");
        for (port, card) in [(first, from), (last, to)] {
            let minimum = card.x + card.width * PORT_MARGIN_FRACTION;
            let maximum = card.x + card.width * (1.0 - PORT_MARGIN_FRACTION);
            assert!(
                port.x >= minimum && port.x <= maximum,
                "horizontal port {} must stay within [{minimum}, {maximum}]",
                port.x
            );
        }
        if from.y + from.height <= to.y {
            assert_eq!(first.y, from.y + from.height);
            assert_eq!(last.y, to.y);
        } else if to.y + to.height <= from.y {
            assert_eq!(first.y, from.y);
            assert_eq!(last.y, to.y + to.height);
        } else {
            let both_top = first.y == from.y && last.y == to.y;
            let both_bottom = first.y == from.y + from.height && last.y == to.y + to.height;
            assert!(
                both_top || both_bottom,
                "overlapping cards use a shared outer edge"
            );
        }
    }

    #[test]
    fn collapsed_module_reference_cycle_does_not_move_the_changed_node() {
        let a = resource("a", &["module.m2"]);
        let m1 = resource("module.m1", &["a"]);
        let m2 = resource("module.m2", &["module.m1"]);
        let desired = vec![a.clone(), m1.clone(), m2.clone()];
        let mut plan = Plan::new();
        plan.add(Effect::Create(a));

        let site = CallSite::new(
            carina_core::resource::EphemeralId::new(ResolvedResourceId::new(
                ResourceId::with_identity("_virtual", "module"),
            )),
            "./modules/cycle",
        );
        let mut trace = ExpansionTrace::new();
        trace.record(PersistentId::new(m1.id.clone()), vec![site.clone()]);
        trace.record(PersistentId::new(m2.id.clone()), vec![site.clone()]);

        let model = model_for(&plan, &desired, Some(&trace));
        let layout = layout_graph(&model);
        let module_id = GraphNodeId::CollapsedModule(site.id);
        let a_rect = layout.node(&GraphNodeId::Effect(0)).expect("changed root");
        let module_rect = layout.node(&module_id).expect("collapsed module");

        assert_eq!(a_rect.column, 0);
        assert_eq!(module_rect.column, a_rect.column);
        assert!(model.ordering_edges.is_empty());
        assert_eq!(model.reference_edges.len(), 2);
        assert!(cycle_removed_ordering_edges(&model).1.is_empty());
        for edge in &model.reference_edges {
            assert!(layout.edge_route(&edge.from, &edge.to).is_some());
        }

        assert_eq!(layout, layout_graph(&model));
    }

    #[test]
    fn delete_of_dependent_runs_before_delete_of_dependency() {
        let mut plan = Plan::new();
        for (binding, dependencies) in [("database", vec![]), ("service", vec!["database"])] {
            let identity = carina_core::resource::ResourceIdentity::try_from(binding.to_string())
                .expect("fixture identity");
            plan.add(Effect::Delete {
                id: ResolvedResourceId::new(ResourceId::with_identity("test.Widget", identity)),
                identifier: AppliedIdentifier::new(format!("{binding}-id")).unwrap(),
                generation: EffectGeneration::Current,
                directives: Default::default(),
                binding: Some(binding.to_string()),
                dependencies: dependencies
                    .into_iter()
                    .map(|dependency| {
                        carina_core::resource::ResolvedResourceId::new(
                            carina_core::resource::ResourceId::with_identity(
                                "test.Widget",
                                dependency,
                            ),
                        )
                    })
                    .collect(),
                explicit_dependencies: Default::default(),
                blocked_by_updates: Default::default(),
            });
        }
        let layout = layout_graph(&model_for(&plan, &[], None));

        assert!(
            layout.node(&GraphNodeId::Effect(1)).unwrap().column
                < layout.node(&GraphNodeId::Effect(0)).unwrap().column,
            "dependent delete must be left of dependency delete"
        );
    }

    #[test]
    fn module_band_boxes_never_overlap() {
        let first = resource("first/leaf", &[]);
        let second = resource("second/leaf", &[]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(first.clone()));
        plan.add(Effect::Create(second.clone()));
        let desired = vec![first.clone(), second.clone()];
        let first_site = CallSite::new(
            carina_core::resource::EphemeralId::new(ResolvedResourceId::new(
                ResourceId::with_identity("_virtual.module", "first"),
            )),
            "./modules/first",
        );
        let second_site = CallSite::new(
            carina_core::resource::EphemeralId::new(ResolvedResourceId::new(
                ResourceId::with_identity("_virtual.module", "second"),
            )),
            "./modules/second",
        );
        let mut trace = ExpansionTrace::new();
        trace.record(PersistentId::new(first.id.clone()), vec![first_site]);
        trace.record(PersistentId::new(second.id.clone()), vec![second_site]);

        let layout = layout_graph(&model_for(&plan, &desired, Some(&trace)));
        assert_eq!(layout.modules.len(), 2);
        let a = &layout.modules[0];
        let b = &layout.modules[1];
        assert!(a.y + a.height <= b.y || b.y + b.height <= a.y);
    }

    #[test]
    fn layout_is_fully_deterministic() {
        let a = resource("a", &[]);
        let b = resource("b", &["a"]);
        let c = resource("c", &["a"]);
        let d = resource("d", &["b", "c"]);
        let desired = vec![a.clone(), b.clone(), c.clone(), d.clone()];
        let mut plan = Plan::new();
        for resource in [d, c, b, a] {
            plan.add(Effect::Create(resource));
        }
        let model = model_for(&plan, &desired, None);

        assert_eq!(layout_graph(&model), layout_graph(&model));
    }

    #[test]
    fn cards_use_individual_content_widths_and_share_their_column_origin() {
        let short = resource("a", &[]);
        let long = resource("a_genuinely_long_resource_binding", &[]);
        let desired = vec![short.clone(), long.clone()];
        let mut plan = Plan::new();
        plan.add(Effect::Create(short));
        plan.add(Effect::Create(long));

        let layout = layout_graph(&model_for(&plan, &desired, None));
        let short = layout.node(&GraphNodeId::Effect(0)).unwrap();
        let long = layout.node(&GraphNodeId::Effect(1)).unwrap();

        assert_eq!(short.column, long.column);
        assert_eq!(short.x, long.x, "cards must be left-aligned in a column");
        assert!(short.width >= 220.0);
        assert!(
            short.width < long.width,
            "each card uses its own content width"
        );
    }

    #[test]
    fn graph_is_left_aligned_with_the_header_margin() {
        let widget = resource("widget", &[]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(widget.clone()));

        let layout = layout_graph(&model_for(&plan, &[widget], None));

        assert_eq!(layout.nodes[0].x, CANVAS_SIDE_MARGIN);
    }

    #[test]
    fn long_edge_route_never_intersects_a_non_endpoint_card() {
        let a = resource("a", &[]);
        let b = resource("b", &["a"]);
        let c = resource("c", &["b"]);
        let d = resource("d", &["a", "c"]);
        let desired = vec![a.clone(), b.clone(), c.clone(), d.clone()];
        let mut plan = Plan::new();
        for resource in [a, b, c, d] {
            plan.add(Effect::Create(resource));
        }
        let model = model_for(&plan, &desired, None);
        let layout = layout_graph(&model);
        let route = layout
            .edge_route(&GraphNodeId::Effect(0), &GraphNodeId::Effect(3))
            .expect("a -> d is the deliberately long ordering edge");

        assert!(
            route.points.len() > 4,
            "long edges must route through at least one intermediate dummy"
        );
        for card in &layout.nodes {
            if card.id != GraphNodeId::Effect(0) && card.id != GraphNodeId::Effect(3) {
                assert!(
                    !edge_route_intersects_rect(route, card),
                    "long edge intersects non-endpoint card {:?}",
                    card.id
                );
            }
        }
    }

    #[test]
    fn long_forward_reference_uses_dummies_and_avoids_intermediate_cards() {
        let a = resource("a", &[]);
        let b = resource("b", &["a"]);
        let c = resource("c", &["b"]);
        let connector = resource("connector", &["a"]);
        let d = resource("d", &["c", "connector"]);
        let connector_id = GraphNodeId::Unchanged(ResolvedResourceId::new(connector.id.clone()));
        let desired = vec![a.clone(), b.clone(), c.clone(), connector, d.clone()];
        let mut plan = Plan::new();
        for resource in [a, b, c, d] {
            plan.add(Effect::Create(resource));
        }
        let model = model_for(&plan, &desired, None);
        let layout = layout_graph(&model);

        let route = layout
            .edge_route(&connector_id, &GraphNodeId::Effect(3))
            .expect("long reference route");
        assert!(route.points.len() > 4, "route must pass through a dummy");
        let intermediate = layout
            .node(&GraphNodeId::Effect(2))
            .expect("intermediate card");
        assert!(
            !edge_route_intersects_rect(route, intermediate),
            "dummy-routed reference intersects the intermediate card"
        );
    }

    #[test]
    fn adjacent_swap_refinement_strictly_reduces_a_nonzero_crossing_fixture() {
        let resources = vec![
            resource("n0", &[]),
            resource("n1", &[]),
            resource("n2", &["n0"]),
            resource("n3", &[]),
            resource("n4", &["n2"]),
            resource("n5", &[]),
            resource("n6", &["n2"]),
            resource("n7", &["n0"]),
            resource("n8", &["n3", "n4", "n5"]),
        ];
        let mut plan = Plan::new();
        for resource in &resources {
            plan.add(Effect::Create(resource.clone()));
        }
        let model = model_for(&plan, &resources, None);

        let (before, after) = crossing_counts_before_and_after_refinement(&model);

        assert_eq!(before, 2, "fixture must retain crossings after barycenter");
        assert_eq!(after, 0, "adjacent swaps must remove those crossings");
    }

    #[test]
    fn fenwick_pair_crossing_count_matches_brute_force_for_random_column_pairs() {
        fn brute_force(
            edges: &[(LayoutVertexId, LayoutVertexId)],
            positions: &HashMap<LayoutVertexId, usize>,
        ) -> usize {
            let mut crossings = 0;
            for (index, (left_from, left_to)) in edges.iter().enumerate() {
                for (right_from, right_to) in &edges[index + 1..] {
                    if left_from == right_from || left_to == right_to {
                        continue;
                    }
                    if (positions[left_from] < positions[right_from])
                        != (positions[left_to] < positions[right_to])
                    {
                        crossings += 1;
                    }
                }
            }
            crossings
        }

        fn next(seed: &mut u64) -> u64 {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *seed
        }

        let mut seed = 0x3966_1234_5678_abcd_u64;
        for case in 0..500 {
            let left_count = (next(&mut seed) % 8 + 1) as usize;
            let right_count = (next(&mut seed) % 8 + 1) as usize;
            let mut left: Vec<_> = (0..left_count)
                .map(|index| LayoutVertexId::Real(GraphNodeId::Effect(index)))
                .collect();
            let mut right: Vec<_> = (0..right_count)
                .map(|index| LayoutVertexId::Real(GraphNodeId::Effect(100 + index)))
                .collect();
            for index in (1..left.len()).rev() {
                let other = next(&mut seed) as usize % (index + 1);
                left.swap(index, other);
            }
            for index in (1..right.len()).rev() {
                let other = next(&mut seed) as usize % (index + 1);
                right.swap(index, other);
            }
            let positions: HashMap<_, _> = left
                .iter()
                .chain(&right)
                .enumerate()
                .map(|(position, id)| (id.clone(), position))
                .collect();
            let mut edges = Vec::new();
            for left_id in &left {
                for right_id in &right {
                    if next(&mut seed).is_multiple_of(3) {
                        edges.push((left_id.clone(), right_id.clone()));
                    }
                }
            }

            assert_eq!(
                pair_crossing_count(&edges, &positions),
                brute_force(&edges, &positions),
                "random case {case}"
            );
        }
    }

    #[test]
    fn coordinate_relaxation_preserves_order_gaps_and_band_bounds() {
        let fixture =
            crate::fixture_plan::build_plan_from_fixture_name("plan_image_module_cascade");
        let summary = crate::fixture_plan::build_fixture_plan_change_summary(&fixture);
        let model = build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &fixture.desired_resources,
            unresolved_resources: &fixture.unresolved_resources,
            compositions: &fixture.compositions,
            expansion_trace: Some(&fixture.expansion_trace),
            state: None,
            summary: &summary,
        });
        let (expanded, positioned, band_bounds) = positioned_vertices_for_test(&model);

        for bounds in &band_bounds {
            for column in 0..expanded.columns.len() {
                let mut vertices: Vec<_> = positioned
                    .iter()
                    .filter(|vertex| {
                        vertex.column == column
                            && expanded.bands.get(&vertex.id) == Some(&bounds.id)
                    })
                    .collect();
                vertices.sort_by(|left, right| left.center_y.total_cmp(&right.center_y));
                for vertex in &vertices {
                    let top = vertex.center_y - vertex.height / 2.0;
                    let bottom = vertex.center_y + vertex.height / 2.0;
                    assert!(top >= bounds.content_top - 0.01);
                    assert!(bottom <= bounds.content_bottom + 0.01);
                }
                for pair in vertices.windows(2) {
                    let upper_bottom = pair[0].center_y + pair[0].height / 2.0;
                    let lower_top = pair[1].center_y - pair[1].height / 2.0;
                    let gap = vertex_gap(&expanded, &pair[0].id, &pair[1].id);
                    assert!(upper_bottom + gap <= lower_top + 0.01);
                }
            }
        }
    }

    #[test]
    fn layout_of_fifteen_hundred_nodes_completes_quickly_in_debug() {
        const HALF: usize = 750;
        let mut desired = Vec::with_capacity(HALF * 2);
        let mut plan = Plan::new();
        for index in 0..HALF {
            let source = resource(&format!("source_{index:04}"), &[]);
            plan.add(Effect::Create(source.clone()));
            desired.push(source);
        }
        for index in 0..HALF {
            let dependency = format!("source_{:04}", HALF - index - 1);
            let target = resource(&format!("target_{index:04}"), &[&dependency]);
            plan.add(Effect::Create(target.clone()));
            desired.push(target);
        }
        let model = model_for(&plan, &desired, None);

        let started = Instant::now();
        let layout = layout_graph(&model);
        let elapsed = started.elapsed();

        assert_eq!(layout.nodes.len(), HALF * 2);
        assert!(
            elapsed < Duration::from_secs(5),
            "1500-node layout took {elapsed:?}"
        );
    }
}
