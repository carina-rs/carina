//! Application state for the TUI plan viewer

use std::collections::{HashMap, HashSet};

use carina_core::detail_rows::{DetailLevel, DetailRow, build_detail_rows};
use carina_core::effect::Effect;
use carina_core::plan::{Plan, PlanSummary};
use carina_core::plan_tree::{
    build_dependency_graph, build_single_parent_tree, extract_compact_hint,
};
use carina_core::resource::ResourceId;
use carina_core::schema::ResourceSchema;
use ratatui::widgets::ListState;

/// A node in the tree view representing one effect
#[derive(Debug)]
pub struct TreeNode {
    /// Effect type label for display
    pub effect_label: String,
    /// Resource type (e.g., "awscc.ec2.Vpc") for display
    pub resource_type: String,
    /// Name part (binding name or compact hint) for display
    pub name_part: String,
    /// Symbol prefix ("+", "~", "-", "+/-", "-/+", "<=")
    pub symbol: String,
    /// The effect kind for coloring
    pub kind: EffectKind,
    /// Detail rows for the detail panel, computed from `build_detail_rows()`
    pub detail_rows: Vec<DetailRow>,
    /// Indices of child nodes in the tree
    pub children: Vec<usize>,
    /// Nesting depth (0 = root)
    pub depth: usize,
    /// Parent node index, if any
    pub parent: Option<usize>,
}

/// Simplified effect kind for coloring
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    Read,
    Create,
    Update,
    Replace,
    Delete,
}

/// Which panel currently has focus
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPanel {
    Tree,
    Detail,
}

/// Application state
pub struct App {
    /// Tree nodes (one per effect)
    pub nodes: Vec<TreeNode>,
    /// Currently selected index in the visible node list
    pub selected: usize,
    /// List state for ratatui scrolling
    pub list_state: ListState,
    /// Plan summary for display (plain text)
    pub summary: String,
    /// Plan summary counts for colored display
    pub plan_summary: PlanSummary,
    /// Which panel currently has focus
    pub focused_panel: FocusedPanel,
    /// Vertical scroll offset for the detail panel
    pub detail_scroll: u16,
    /// Scroll offset for the tree panel (index of item at top of visible area)
    pub tree_scroll_offset: usize,
    /// Height of the tree panel's inner area (updated each frame)
    pub tree_area_height: usize,
    /// Whether search mode is active (user is typing a query)
    pub search_active: bool,
    /// Current search query string
    pub search_query: String,
    /// Indices of visible nodes that match the search query
    pub search_matches: Vec<usize>,
    /// Index into `search_matches` for the current match
    pub current_match: usize,
    /// Selected row index in the detail panel (for attribute navigation)
    pub detail_selected: usize,
    /// Navigation history stack (stores absolute node indices for back navigation)
    pub nav_stack: Vec<usize>,
    /// Tab completion candidates (sorted, unique)
    tab_candidates: Vec<String>,
    /// Current index into tab_candidates for cycling
    tab_index: usize,
    /// The query prefix that was used for the current tab completion cycle
    tab_prefix: String,
}

impl App {
    pub fn new(plan: &Plan, schemas: &HashMap<String, ResourceSchema>) -> Self {
        let schemas_opt = if schemas.is_empty() {
            None
        } else {
            Some(schemas)
        };
        let mut nodes: Vec<TreeNode> = plan
            .effects()
            .iter()
            .map(|e| effect_to_node(e, schemas_opt))
            .collect();
        let plan_summary = plan.summary();
        let summary = format!("{}", plan_summary);

        // Build tree structure from dependency analysis
        build_tree_structure(plan, &mut nodes);

        // Shorten effect labels: strip provider prefix, use binding or compact hint
        shorten_effect_labels(plan, &mut nodes);

        // Suppress Move nodes when an Update/Replace exists for the same target,
        // matching CLI behavior (the move info is shown as annotation on Update/Replace).
        let update_or_replace_targets: HashSet<ResourceId> = plan
            .effects()
            .iter()
            .filter_map(|e| match e {
                Effect::Update { id, .. } | Effect::Replace { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();

        let suppressed: HashSet<usize> = plan
            .effects()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if let Effect::Move { to, .. } = e
                    && update_or_replace_targets.contains(to)
                {
                    return Some(i);
                }
                None
            })
            .collect();

        if !suppressed.is_empty() {
            // Build old→new index mapping for remapping parent/children references
            let index_map: Vec<Option<usize>> = {
                let mut map = Vec::with_capacity(nodes.len());
                let mut new_idx = 0usize;
                for old_idx in 0..nodes.len() {
                    if suppressed.contains(&old_idx) {
                        map.push(None);
                    } else {
                        map.push(Some(new_idx));
                        new_idx += 1;
                    }
                }
                map
            };

            let mut old_idx = 0;
            nodes.retain(|_| {
                let keep = !suppressed.contains(&old_idx);
                old_idx += 1;
                keep
            });

            for node in &mut nodes {
                node.parent = node.parent.and_then(|p| index_map[p]);
                node.children = node.children.iter().filter_map(|&c| index_map[c]).collect();
            }
        }

        let mut list_state = ListState::default();
        if !nodes.is_empty() {
            list_state.select(Some(0));
        }
        App {
            nodes,
            selected: 0,
            list_state,
            summary,
            plan_summary,
            focused_panel: FocusedPanel::Tree,
            detail_scroll: 0,
            tree_scroll_offset: 0,
            tree_area_height: 0,
            detail_selected: 0,
            nav_stack: Vec::new(),
            search_active: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            current_match: 0,
            tab_candidates: Vec::new(),
            tab_index: 0,
            tab_prefix: String::new(),
        }
    }

    /// Returns all node indices in DFS tree order.
    ///
    /// When a search query is active, only matching nodes and their ancestors
    /// are included (filter mode). Otherwise, all nodes are returned.
    pub fn visible_nodes(&self) -> Vec<usize> {
        let all_dfs = self.all_nodes_dfs();
        if self.search_query.is_empty() {
            return all_dfs;
        }
        // Compute the set of matching node indices and their ancestors
        let match_set = self.matching_node_indices();
        if match_set.is_empty() {
            return all_dfs;
        }
        let mut visible_set: HashSet<usize> = HashSet::new();
        for &idx in &match_set {
            visible_set.insert(idx);
            // Walk up ancestors
            let mut cur = self.nodes[idx].parent;
            while let Some(p) = cur {
                if !visible_set.insert(p) {
                    break; // already added this ancestor chain
                }
                cur = self.nodes[p].parent;
            }
        }
        all_dfs
            .into_iter()
            .filter(|idx| visible_set.contains(idx))
            .collect()
    }

    /// Returns all node indices in DFS order (unfiltered).
    fn all_nodes_dfs(&self) -> Vec<usize> {
        let mut result = Vec::new();
        for (idx, node) in self.nodes.iter().enumerate() {
            if node.parent.is_none() {
                Self::collect_dfs(idx, &self.nodes, &mut result);
            }
        }
        result
    }

    fn collect_dfs(idx: usize, nodes: &[TreeNode], result: &mut Vec<usize>) {
        result.push(idx);
        for &child in &nodes[idx].children {
            Self::collect_dfs(child, nodes, result);
        }
    }

    /// Number of visible nodes
    pub fn visible_count(&self) -> usize {
        self.visible_nodes().len()
    }

    /// Returns the set of node indices whose effect_label matches the search query.
    fn matching_node_indices(&self) -> HashSet<usize> {
        let mut result = HashSet::new();
        if self.search_query.is_empty() {
            return result;
        }
        let query_lower = self.search_query.to_lowercase();
        for (idx, node) in self.nodes.iter().enumerate() {
            if node.effect_label.to_lowercase().contains(&query_lower) {
                result.insert(idx);
            }
        }
        result
    }

    /// Returns whether a node index is an "ancestor-only" node (shown dimmed).
    ///
    /// A node is ancestor-only if it's visible only because it's an ancestor
    /// of a matching node, but doesn't match the query itself.
    pub fn is_ancestor_only(&self, node_idx: usize) -> bool {
        if self.search_query.is_empty() {
            return false;
        }
        let query_lower = self.search_query.to_lowercase();
        !self.nodes[node_idx]
            .effect_label
            .to_lowercase()
            .contains(&query_lower)
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.tree_scroll_offset {
                self.tree_scroll_offset = self.selected;
            }
            self.sync_list_state();
            self.detail_scroll = 0;
            self.detail_selected = 0;
        }
    }

    pub fn move_down(&mut self) {
        let count = self.visible_count();
        if count > 0 && self.selected < count - 1 {
            self.selected += 1;
            if self.tree_area_height > 0
                && self.selected >= self.tree_scroll_offset + self.tree_area_height
            {
                self.tree_scroll_offset = self.selected - self.tree_area_height + 1;
            }
            self.sync_list_state();
            self.detail_scroll = 0;
            self.detail_selected = 0;
        }
    }

    /// Sync `list_state` selection and scroll offset to match our manual tracking (public for tests).
    #[cfg(test)]
    pub fn sync_list_state_pub(&mut self) {
        self.sync_list_state();
    }

    /// Sync `list_state` selection and scroll offset to match our manual tracking.
    fn sync_list_state(&mut self) {
        self.list_state.select(Some(self.selected));
        *self.list_state.offset_mut() = self.tree_scroll_offset;
    }

    /// Toggle focus between Tree and Detail panels
    pub fn toggle_focus(&mut self) {
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Tree => FocusedPanel::Detail,
            FocusedPanel::Detail => FocusedPanel::Tree,
        };
    }

    /// Scroll the detail panel up by one line
    pub fn detail_scroll_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

    /// Scroll the detail panel down by one line
    pub fn detail_scroll_down(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_add(1);
    }

    /// Move detail selection up by one row
    pub fn detail_select_up(&mut self) {
        if self.detail_selected > 0 {
            self.detail_selected -= 1;
        }
    }

    /// Move detail selection down by one row
    pub fn detail_select_down(&mut self) {
        if let Some(node) = self.selected_node() {
            let max = node.detail_rows.len().saturating_sub(1);
            if self.detail_selected < max {
                self.detail_selected += 1;
            }
        }
    }

    /// Get the ref_binding of the currently selected detail row, if any
    pub fn selected_detail_ref_binding(&self) -> Option<String> {
        let node = self.selected_node()?;
        let row = node.detail_rows.get(self.detail_selected)?;
        match row {
            DetailRow::Attribute { ref_binding, .. } => ref_binding.clone(),
            _ => None,
        }
    }

    /// Follow a ResourceRef: find the node with the given binding name,
    /// push current node onto nav_stack, and jump to the referenced node.
    /// Returns true if the jump was successful.
    pub fn follow_ref(&mut self, binding: &str) -> bool {
        // Find the node whose binding matches
        let target_node_idx = self.nodes.iter().enumerate().find_map(|(idx, node)| {
            // Check if this node's name_part matches the binding
            if node.name_part == binding {
                Some(idx)
            } else {
                None
            }
        });

        if let Some(target_idx) = target_node_idx {
            // Push current node onto nav_stack
            if let Some(current_idx) = self.selected_node_idx() {
                self.nav_stack.push(current_idx);
            }
            // Find the target in the visible list and jump to it
            let visible = self.visible_nodes();
            if let Some(vis_pos) = visible.iter().position(|&idx| idx == target_idx) {
                self.select_visible_index(vis_pos);
                self.detail_selected = 0;
                return true;
            }
        }
        false
    }

    /// Navigate back to the previous node in the nav_stack.
    /// Returns true if the jump was successful.
    pub fn nav_back(&mut self) -> bool {
        if let Some(prev_idx) = self.nav_stack.pop() {
            let visible = self.visible_nodes();
            if let Some(vis_pos) = visible.iter().position(|&idx| idx == prev_idx) {
                self.select_visible_index(vis_pos);
                self.detail_selected = 0;
                return true;
            }
        }
        false
    }

    /// Get the node index for the currently selected visible row
    pub fn selected_node_idx(&self) -> Option<usize> {
        let nodes = self.visible_nodes();
        nodes.get(self.selected).copied()
    }

    /// Get the currently selected node, if any
    pub fn selected_node(&self) -> Option<&TreeNode> {
        self.selected_node_idx().map(|idx| &self.nodes[idx])
    }

    /// Restore the selection to a previously saved absolute node index.
    ///
    /// Finds the position of `saved_node` in the current `visible_nodes()` list
    /// and sets `selected` accordingly. Falls back to clamping if the node is
    /// not found.
    pub fn restore_selection(&mut self, saved_node: Option<usize>) {
        let visible = self.visible_nodes();
        if let Some(node_idx) = saved_node {
            if let Some(pos) = visible.iter().position(|&idx| idx == node_idx) {
                self.selected = pos;
            } else {
                // Node not in visible list; clamp to last
                self.selected = visible.len().saturating_sub(1);
            }
        } else {
            self.selected = 0;
        }
        // Adjust scroll so the selected item is visible
        if self.selected < self.tree_scroll_offset {
            self.tree_scroll_offset = self.selected;
        } else if self.tree_area_height > 0
            && self.selected >= self.tree_scroll_offset + self.tree_area_height
        {
            self.tree_scroll_offset = self.selected - self.tree_area_height + 1;
        }
        self.sync_list_state();
        self.detail_scroll = 0;
    }

    /// Update search matches based on the current query.
    ///
    /// Matches against each node's `effect_label` (which contains both
    /// the resource type and the name), case-insensitively.
    /// In filter mode, search_matches contains indices into the filtered
    /// visible_nodes() list, pointing only to actual matches (not ancestors).
    pub fn update_search_matches(&mut self) {
        self.search_matches.clear();
        self.current_match = 0;
        // Reset tab completion state when query changes
        self.tab_candidates.clear();
        self.tab_index = 0;
        self.tab_prefix.clear();
        if self.search_query.is_empty() {
            return;
        }
        let query_lower = self.search_query.to_lowercase();
        let visible = self.visible_nodes();
        for (vis_idx, &node_idx) in visible.iter().enumerate() {
            let node = &self.nodes[node_idx];
            if node.effect_label.to_lowercase().contains(&query_lower) {
                self.search_matches.push(vis_idx);
            }
        }
    }

    /// Jump to the match at `current_match` index, updating selection and scroll.
    pub fn jump_to_current_match(&mut self) {
        if let Some(&vis_idx) = self.search_matches.get(self.current_match) {
            self.select_visible_index(vis_idx);
        }
    }

    /// Jump to the next search match. Wraps around to the first match.
    pub fn next_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.current_match = (self.current_match + 1) % self.search_matches.len();
        self.jump_to_current_match();
    }

    /// Jump to the previous search match. Wraps around to the last match.
    pub fn prev_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        if self.current_match == 0 {
            self.current_match = self.search_matches.len() - 1;
        } else {
            self.current_match -= 1;
        }
        self.jump_to_current_match();
    }

    /// Select a specific visible index and adjust scroll.
    fn select_visible_index(&mut self, vis_idx: usize) {
        self.selected = vis_idx;
        // Adjust scroll so the selected item is visible
        if self.selected < self.tree_scroll_offset {
            self.tree_scroll_offset = self.selected;
        } else if self.tree_area_height > 0
            && self.selected >= self.tree_scroll_offset + self.tree_area_height
        {
            self.tree_scroll_offset = self.selected - self.tree_area_height + 1;
        }
        self.sync_list_state();
        self.detail_scroll = 0;
        self.detail_selected = 0;
    }

    /// Perform tab completion on the current search query.
    ///
    /// Collects unique resource type names and binding/display names from all
    /// tree nodes, then completes from matching candidates. Subsequent Tab
    /// presses cycle through candidates.
    pub fn tab_complete(&mut self) {
        // Check if we're in an active tab-cycling session.
        // We're cycling if candidates exist and the current query matches
        // the last completed candidate (meaning user hasn't typed anything new).
        let is_cycling = !self.tab_candidates.is_empty()
            && self
                .tab_candidates
                .get(self.tab_index)
                .map(|c| *c == self.search_query)
                .unwrap_or(false);

        if is_cycling {
            // Cycle to next candidate
            self.tab_index = (self.tab_index + 1) % self.tab_candidates.len();
        } else {
            // Build new candidate list from current query
            self.tab_prefix = self.search_query.clone();
            self.tab_index = 0;

            let prefix_lower = self.search_query.to_lowercase();
            if prefix_lower.is_empty() {
                self.tab_candidates.clear();
                return;
            }

            let mut candidates: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for node in &self.nodes {
                // Resource type name (e.g., "ec2.Vpc" or "awscc.ec2.Vpc")
                // Use contains() to match anywhere in the dotted name,
                // consistent with how update_search_matches uses contains()
                let rt_lower = node.resource_type.to_lowercase();
                if rt_lower.contains(&prefix_lower) && seen.insert(rt_lower) {
                    candidates.push(node.resource_type.clone());
                }
                // Binding/display name (e.g., "vpc", "subnet")
                let np_lower = node.name_part.to_lowercase();
                if np_lower.contains(&prefix_lower) && seen.insert(np_lower) {
                    candidates.push(node.name_part.clone());
                }
            }
            candidates.sort_by_key(|a| a.to_lowercase());
            self.tab_candidates = candidates;
        }

        if let Some(candidate) = self.tab_candidates.get(self.tab_index) {
            self.search_query = candidate.clone();
            // Don't reset tab state when updating matches for tab completion
            let saved_candidates = std::mem::take(&mut self.tab_candidates);
            let saved_index = self.tab_index;
            let saved_prefix = std::mem::take(&mut self.tab_prefix);
            self.update_search_matches();
            self.tab_candidates = saved_candidates;
            self.tab_index = saved_index;
            self.tab_prefix = saved_prefix;
            if !self.search_matches.is_empty() {
                self.jump_to_current_match();
            }
        }
    }
}

/// Build tree structure by analyzing dependencies between effects.
///
/// This reuses the same dependency-based algorithm from `carina-cli/src/display.rs`:
/// - Builds forward/reverse dependency maps from `ResourceRef` attributes
/// - Assigns each resource a single parent (shallowest dependency)
/// - Sorts siblings by `(resource_type, binding_name)`
fn build_tree_structure(plan: &Plan, nodes: &mut [TreeNode]) {
    if nodes.is_empty() {
        return;
    }

    // Build dependency graph and single-parent tree using shared logic
    let graph = build_dependency_graph(plan);
    let (roots, dependents) = build_single_parent_tree(plan, &graph);

    // Step 3: Compute depth and parent/children relationships via DFS
    fn assign_tree(
        idx: usize,
        depth: usize,
        parent: Option<usize>,
        dependents: &HashMap<usize, Vec<usize>>,
        nodes: &mut [TreeNode],
    ) {
        nodes[idx].depth = depth;
        nodes[idx].parent = parent;
        let children = dependents.get(&idx).cloned().unwrap_or_default();
        nodes[idx].children = children.clone();
        for child in children {
            assign_tree(child, depth + 1, Some(idx), dependents, nodes);
        }
    }

    for &root in &roots {
        assign_tree(root, 0, None, &dependents, nodes);
    }

    // Nodes not reached by the tree (e.g., Delete effects with no deps) remain
    // roots with depth 0, parent None, and no children -- the defaults are correct.
}

/// Shorten effect labels: strip provider prefix and use binding name or compact hint.
fn shorten_effect_labels(plan: &Plan, nodes: &mut [TreeNode]) {
    for (idx, effect) in plan.effects().iter().enumerate() {
        let resource = match effect {
            Effect::Create(r) => Some(r),
            Effect::Update { to, .. } => Some(to),
            Effect::Replace { to, .. } => Some(to),
            Effect::Read { resource } => Some(resource),
            Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. } => None,
        };

        if let Some(r) = resource {
            let display_type = r.id.display_type();
            let has_binding = r.binding.is_some();

            let name_part = if has_binding {
                // For bound resources, show the binding name
                r.binding
                    .clone()
                    .unwrap_or_else(|| r.id.name_str().to_string())
            } else {
                // For anonymous resources, try to extract a compact hint
                let parent_binding = nodes[idx].parent.and_then(|p_idx| {
                    let p_effect = &plan.effects()[p_idx];
                    if let Effect::Delete { binding, .. } = p_effect {
                        return binding.clone();
                    }
                    let p_resource = match p_effect {
                        Effect::Create(r) => Some(r),
                        Effect::Update { to, .. } => Some(to),
                        Effect::Replace { to, .. } => Some(to),
                        Effect::Read { resource } => Some(resource),
                        Effect::Delete { .. }
                        | Effect::Import { .. }
                        | Effect::Remove { .. }
                        | Effect::Move { .. } => None,
                    };
                    p_resource.and_then(|pr| pr.binding.clone())
                });
                if let Some(hint) = extract_compact_hint(r, parent_binding.as_deref()) {
                    format!("({})", hint)
                } else {
                    r.id.name_str().to_string()
                }
            };

            nodes[idx].resource_type = display_type.clone();
            nodes[idx].name_part = name_part.clone();
            nodes[idx].effect_label = format!("{} {}", display_type, name_part);
        } else if let Effect::Delete { id, .. } = effect {
            let display_type = id.display_type();
            nodes[idx].resource_type = display_type.clone();
            nodes[idx].name_part = id.name_str().to_string();
            nodes[idx].effect_label = format!("{} {}", display_type, id.name);
        }
    }
}

fn effect_to_node(effect: &Effect, schemas: Option<&HashMap<String, ResourceSchema>>) -> TreeNode {
    let detail_rows = build_detail_rows(effect, schemas, DetailLevel::Full, None);

    match effect {
        Effect::Read { resource } => TreeNode {
            effect_label: format!("{}", resource.id),
            resource_type: resource.id.display_type(),
            name_part: resource.id.name_str().to_string(),
            symbol: "<=".to_string(),
            kind: EffectKind::Read,
            detail_rows,
            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Create(resource) => TreeNode {
            effect_label: format!("{}", resource.id),
            resource_type: resource.id.display_type(),
            name_part: resource.id.name_str().to_string(),
            symbol: "+".to_string(),
            kind: EffectKind::Create,
            detail_rows,
            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Update { id, .. } => TreeNode {
            effect_label: format!("{}", id),
            resource_type: id.display_type(),
            name_part: id.name_str().to_string(),
            symbol: "~".to_string(),
            kind: EffectKind::Update,
            detail_rows,
            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Replace { id, lifecycle, .. } => {
            let symbol = if lifecycle.create_before_destroy {
                "+/-".to_string()
            } else {
                "-/+".to_string()
            };
            TreeNode {
                effect_label: format!("{}", id),
                resource_type: id.display_type(),
                name_part: id.name_str().to_string(),
                symbol,
                kind: EffectKind::Replace,
                detail_rows,
                children: Vec::new(),
                depth: 0,
                parent: None,
            }
        }
        Effect::Delete { id, identifier, .. } => {
            // build_detail_rows returns empty for Delete without delete_attributes,
            // so add the identifier as a manual attribute row
            let mut rows = detail_rows;
            if rows.is_empty() && !identifier.is_empty() {
                rows.push(DetailRow::Attribute {
                    key: "identifier".to_string(),
                    value: identifier.clone(),
                    ref_binding: None,
                    annotation: None,
                });
            }
            TreeNode {
                effect_label: format!("{}", id),
                resource_type: id.display_type(),
                name_part: id.name_str().to_string(),
                symbol: "-".to_string(),
                kind: EffectKind::Delete,
                detail_rows: rows,
                children: Vec::new(),
                depth: 0,
                parent: None,
            }
        }
        Effect::Import { id, .. } => TreeNode {
            effect_label: format!("{}", id),
            resource_type: id.display_type(),
            name_part: id.name_str().to_string(),
            symbol: "<-".to_string(),
            kind: EffectKind::Read,
            detail_rows,
            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Remove { id } => TreeNode {
            effect_label: format!("{}", id),
            resource_type: id.display_type(),
            name_part: id.name_str().to_string(),
            symbol: "x".to_string(),
            kind: EffectKind::Delete,
            detail_rows,
            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Move { from, to } => TreeNode {
            effect_label: format!("{} -> {}", from, to),
            resource_type: to.display_type(),
            name_part: to.name_str().to_string(),
            symbol: "->".to_string(),
            kind: EffectKind::Update,
            detail_rows,
            children: Vec::new(),
            depth: 0,
            parent: None,
        },
    }
}

#[cfg(test)]
mod tests;
