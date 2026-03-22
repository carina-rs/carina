//! Application state for the TUI plan viewer

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

use carina_core::deps::get_resource_dependencies;
use carina_core::effect::Effect;
use carina_core::plan::{Plan, PlanSummary};
use carina_core::resource::Value;
use carina_core::schema::ResourceSchema;
use ratatui::widgets::ListState;

/// A node in the tree view representing one effect
#[derive(Debug)]
pub struct TreeNode {
    /// Effect type label for display
    pub effect_label: String,
    /// Resource type (e.g., "awscc.ec2.vpc") for display
    pub resource_type: String,
    /// Name part (binding name or compact hint) for display
    pub name_part: String,
    /// Symbol prefix ("+", "~", "-", "+/-", "-/+", "<=")
    pub symbol: String,
    /// The effect kind for coloring
    pub kind: EffectKind,
    /// Attributes to show in the detail panel (key -> display value)
    pub attributes: Vec<(String, String)>,
    /// For Update effects, the set of changed attribute names
    pub changed_attributes: Vec<String>,
    /// For Update/Replace effects, the "from" attributes (old values)
    pub from_attributes: Vec<(String, String)>,
    /// Raw "from" attribute values for map key-level diffs in the detail panel
    pub raw_from_attrs: HashMap<String, Value>,
    /// Raw "to" attribute values for map key-level diffs in the detail panel
    pub raw_to_attrs: HashMap<String, Value>,
    /// Default value attributes from schema (attr_name, formatted_value) for Create effects
    pub default_attributes: Vec<(String, String)>,
    /// Read-only attributes from schema (attr_name) for Create effects
    pub read_only_attributes: Vec<String>,
    /// Count of unchanged attributes for Update/Replace effects
    pub unchanged_count: usize,
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
}

impl App {
    pub fn new(plan: &Plan, schemas: &HashMap<String, ResourceSchema>) -> Self {
        let mut nodes: Vec<TreeNode> = plan.effects().iter().map(effect_to_node).collect();
        let plan_summary = plan.summary();
        let summary = format!("{}", plan_summary);

        // Populate schema-derived attributes
        populate_schema_attributes(plan, &mut nodes, schemas);

        // Build tree structure from dependency analysis
        build_tree_structure(plan, &mut nodes);

        // Shorten effect labels: strip provider prefix, use binding or compact hint
        shorten_effect_labels(plan, &mut nodes);

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
            search_active: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            current_match: 0,
        }
    }

    /// Returns all node indices in DFS tree order (all nodes always visible).
    pub fn visible_nodes(&self) -> Vec<usize> {
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

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.tree_scroll_offset {
                self.tree_scroll_offset = self.selected;
            }
            self.sync_list_state();
            self.detail_scroll = 0;
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
        }
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

    /// Get the node index for the currently selected visible row
    pub fn selected_node_idx(&self) -> Option<usize> {
        let nodes = self.visible_nodes();
        nodes.get(self.selected).copied()
    }

    /// Get the currently selected node, if any
    pub fn selected_node(&self) -> Option<&TreeNode> {
        self.selected_node_idx().map(|idx| &self.nodes[idx])
    }

    /// Update search matches based on the current query.
    ///
    /// Matches against each node's `effect_label` (which contains both
    /// the resource type and the name), case-insensitively.
    pub fn update_search_matches(&mut self) {
        self.search_matches.clear();
        self.current_match = 0;
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

    // Step 1: Build dependency maps from effects
    let mut binding_to_effect: HashMap<String, usize> = HashMap::new();
    let mut effect_deps: HashMap<usize, HashSet<String>> = HashMap::new();
    let mut effect_bindings: HashMap<usize, String> = HashMap::new();
    let mut effect_types: HashMap<usize, String> = HashMap::new();

    for (idx, effect) in plan.effects().iter().enumerate() {
        let (resource, deps) = match effect {
            Effect::Create(r) => (Some(r), get_resource_dependencies(r)),
            Effect::Update { to, .. } => (Some(to), get_resource_dependencies(to)),
            Effect::Replace { to, .. } => (Some(to), get_resource_dependencies(to)),
            Effect::Read { resource } => (Some(resource), get_resource_dependencies(resource)),
            Effect::Delete {
                id,
                binding,
                dependencies,
                ..
            } => {
                let deps = dependencies.clone();
                if let Some(b) = binding {
                    binding_to_effect.insert(b.clone(), idx);
                    effect_bindings.insert(idx, b.clone());
                } else {
                    let fallback = id.to_string();
                    binding_to_effect.insert(fallback.clone(), idx);
                    effect_bindings.insert(idx, fallback);
                }
                effect_types.insert(idx, id.resource_type.clone());
                effect_deps.insert(idx, deps);
                continue;
            }
        };

        if let Some(r) = resource {
            let binding = r
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| r.id.to_string());
            binding_to_effect.insert(binding.clone(), idx);
            effect_bindings.insert(idx, binding);
            effect_types.insert(idx, r.id.resource_type.clone());
        }
        effect_deps.insert(idx, deps);
    }

    // Step 2: Build the single-parent tree
    let (roots, dependents) = build_single_parent_tree(
        plan,
        &binding_to_effect,
        &effect_deps,
        &effect_bindings,
        &effect_types,
    );

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

/// Build a single-parent tree from the dependency graph.
///
/// Replicates the algorithm from `carina-cli/src/display.rs`.
fn build_single_parent_tree(
    plan: &Plan,
    binding_to_effect: &HashMap<String, usize>,
    effect_deps: &HashMap<usize, HashSet<String>>,
    effect_bindings: &HashMap<usize, String>,
    effect_types: &HashMap<usize, String>,
) -> (Vec<usize>, HashMap<usize, Vec<usize>>) {
    let effect_binding_set: HashSet<&str> = binding_to_effect.keys().map(|s| s.as_str()).collect();

    let sort_key = |idx: &usize| -> (String, String) {
        let rtype = effect_types.get(idx).cloned().unwrap_or_default();
        let binding = effect_bindings.get(idx).cloned().unwrap_or_default();
        (rtype, binding)
    };

    // Build the full reverse dependency map (all parents)
    let mut all_dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        all_dependents.insert(idx, Vec::new());
    }
    for (idx, deps) in effect_deps {
        for dep in deps {
            if let Some(&dep_idx) = binding_to_effect.get(dep) {
                all_dependents.entry(dep_idx).or_default().push(*idx);
            }
        }
    }

    // Identify initial roots (no deps in plan)
    let mut initial_roots: Vec<usize> = Vec::new();
    for (idx, deps) in effect_deps {
        let has_dep_in_plan = deps.iter().any(|d| binding_to_effect.contains_key(d));
        if !has_dep_in_plan {
            initial_roots.push(*idx);
        }
    }
    initial_roots.sort();

    // Nest no-dep resources under their first dependent (Issue #928)
    let mut nested_under_dependent: HashSet<usize> = HashSet::new();
    for &idx in &initial_roots {
        let mut children = all_dependents.get(&idx).cloned().unwrap_or_default();
        children.sort_by_key(|a| sort_key(a));
        if !children.is_empty() {
            let binding_of_idx = effect_bindings.get(&idx).map(|s| s.as_str());
            let all_dependents_have_other_deps = children.iter().all(|&child_idx| {
                effect_deps.get(&child_idx).is_some_and(|child_deps| {
                    child_deps.iter().any(|d| {
                        effect_binding_set.contains(d.as_str())
                            && Some(d.as_str()) != binding_of_idx
                    })
                })
            });
            if all_dependents_have_other_deps {
                nested_under_dependent.insert(idx);
            }
        }
    }

    // Compute final roots
    let roots: Vec<usize> = initial_roots
        .iter()
        .filter(|idx| !nested_under_dependent.contains(idx))
        .cloned()
        .collect();

    // Compute depth for each resource via BFS from roots
    let mut depth: HashMap<usize, usize> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &root in &roots {
        depth.insert(root, 0);
        queue.push_back(root);
    }
    while let Some(node) = queue.pop_front() {
        let d = depth[&node];
        if let Some(children) = all_dependents.get(&node) {
            for &child in children {
                if let std::collections::hash_map::Entry::Vacant(e) = depth.entry(child) {
                    e.insert(d + 1);
                    queue.push_back(child);
                }
            }
        }
    }

    // For each non-root resource, select a single parent
    let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        dependents.insert(idx, Vec::new());
    }

    for (idx, deps) in effect_deps {
        if roots.contains(idx) || nested_under_dependent.contains(idx) {
            continue;
        }
        let mut parent_candidates: Vec<usize> = deps
            .iter()
            .filter_map(|d| binding_to_effect.get(d).cloned())
            .collect();
        if parent_candidates.is_empty() {
            continue;
        }
        parent_candidates.sort_by(|a, b| {
            let da = depth.get(a).copied().unwrap_or(usize::MAX);
            let db = depth.get(b).copied().unwrap_or(usize::MAX);
            da.cmp(&db).then_with(|| sort_key(a).cmp(&sort_key(b)))
        });
        let parent = parent_candidates[0];
        dependents.entry(parent).or_default().push(*idx);
    }

    // Add nested-under-dependent resources as children of the shallowest referencing resource
    for &idx in &nested_under_dependent {
        let mut children = all_dependents.get(&idx).cloned().unwrap_or_default();
        children.sort_by(|a, b| {
            let da = depth.get(a).copied().unwrap_or(usize::MAX);
            let db = depth.get(b).copied().unwrap_or(usize::MAX);
            da.cmp(&db).then_with(|| sort_key(a).cmp(&sort_key(b)))
        });
        if let Some(&best_dependent) = children.first() {
            dependents.entry(best_dependent).or_default().push(idx);
        }
    }

    // Sort each parent's children
    for children in dependents.values_mut() {
        children.sort_by_key(|a| sort_key(a));
    }

    // Sort roots
    let mut sorted_roots = roots;
    sorted_roots.sort_by_key(|a| sort_key(a));

    (sorted_roots, dependents)
}

/// Shorten effect labels: strip provider prefix and use binding name or compact hint.
fn shorten_effect_labels(plan: &Plan, nodes: &mut [TreeNode]) {
    for (idx, effect) in plan.effects().iter().enumerate() {
        let resource = match effect {
            Effect::Create(r) => Some(r),
            Effect::Update { to, .. } => Some(to),
            Effect::Replace { to, .. } => Some(to),
            Effect::Read { resource } => Some(resource),
            Effect::Delete { .. } => None,
        };

        if let Some(r) = resource {
            let display_type = r.id.display_type();
            let has_binding = r.attributes.contains_key("_binding");

            let name_part = if has_binding {
                // For bound resources, show the binding name
                r.attributes
                    .get("_binding")
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| r.id.name.clone())
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
                        Effect::Delete { .. } => None,
                    };
                    p_resource.and_then(|pr| {
                        pr.attributes.get("_binding").and_then(|v| match v {
                            Value::String(s) => Some(s.clone()),
                            _ => None,
                        })
                    })
                });
                if let Some(hint) = extract_compact_hint(r, parent_binding.as_deref()) {
                    format!("({})", hint)
                } else {
                    r.id.name.clone()
                }
            };

            nodes[idx].resource_type = display_type.clone();
            nodes[idx].name_part = name_part.clone();
            nodes[idx].effect_label = format!("{} {}", display_type, name_part);
        } else if let Effect::Delete { id, .. } = effect {
            let display_type = id.display_type();
            nodes[idx].resource_type = display_type.clone();
            nodes[idx].name_part = id.name.clone();
            nodes[idx].effect_label = format!("{} {}", display_type, id.name);
        }
    }
}

/// Extract a compact hint for anonymous resources (mirrors CLI logic).
fn extract_compact_hint(
    resource: &carina_core::resource::Resource,
    parent_binding: Option<&str>,
) -> Option<String> {
    let mut keys: Vec<_> = resource
        .attributes
        .keys()
        .filter(|k| !k.starts_with('_'))
        .collect();
    keys.sort();

    // Priority 1: First distinguishing string attribute
    for key in &keys {
        if let Some(Value::String(s)) = resource.attributes.get(*key)
            && !s.is_empty()
        {
            let short_key = shorten_attr_name(key);
            let display_value = shorten_service_name(key, s);
            return Some(format!("{}: {}", short_key, display_value));
        }
    }

    // Priority 2: First non-parent ResourceRef attribute
    for key in &keys {
        match resource.attributes.get(*key) {
            Some(Value::ResourceRef { binding_name, .. }) => {
                if parent_binding == Some(binding_name.as_str()) {
                    continue;
                }
                let short_key = shorten_attr_name(key);
                return Some(format!("{}: {}", short_key, binding_name));
            }
            Some(Value::List(items)) => {
                for item in items {
                    if let Value::ResourceRef { binding_name, .. } = item {
                        if parent_binding == Some(binding_name.as_str()) {
                            continue;
                        }
                        let short_key = shorten_attr_name(key);
                        return Some(format!("{}: {}", short_key, binding_name));
                    }
                }
            }
            _ => {}
        }
    }

    None
}

/// Shorten common attribute name suffixes for compact display.
fn shorten_attr_name(attr: &str) -> &str {
    attr.strip_suffix("_ids")
        .or_else(|| attr.strip_suffix("_id"))
        .or_else(|| attr.strip_suffix("_name"))
        .unwrap_or(attr)
}

/// For `service_name` attributes, extract just the service suffix from AWS endpoint names.
fn shorten_service_name<'a>(attr_name: &str, value: &'a str) -> Cow<'a, str> {
    if attr_name == "service_name"
        && let Some(rest) = value.strip_prefix("com.amazonaws.")
        && let Some(dot_pos) = rest.find('.')
    {
        let after_region = &rest[dot_pos + 1..];
        if !after_region.is_empty() {
            return Cow::Borrowed(after_region);
        }
    }
    Cow::Borrowed(value)
}

/// Populate schema-derived attributes (defaults, read-only, unchanged count) on tree nodes.
fn populate_schema_attributes(
    plan: &Plan,
    nodes: &mut [TreeNode],
    schemas: &HashMap<String, ResourceSchema>,
) {
    for (idx, effect) in plan.effects().iter().enumerate() {
        match effect {
            Effect::Create(r) => {
                let schema_key = r.id.display_type();
                if let Some(schema) = schemas.get(&schema_key) {
                    let user_keys: HashSet<&str> = r
                        .attributes
                        .keys()
                        .filter(|k| !k.starts_with('_'))
                        .map(|k| k.as_str())
                        .collect();

                    // Default value attributes not specified by user
                    let mut default_attrs: Vec<(&str, &Value)> = schema
                        .default_value_attributes()
                        .into_iter()
                        .filter(|(a, _)| !user_keys.contains(a))
                        .collect();
                    default_attrs.sort_by_key(|(a, _)| *a);
                    nodes[idx].default_attributes = default_attrs
                        .into_iter()
                        .map(|(name, val)| (name.to_string(), format_value(val)))
                        .collect();

                    // Read-only attributes not specified by user
                    let mut ro_attrs: Vec<&str> = schema
                        .read_only_attributes()
                        .into_iter()
                        .filter(|a| !user_keys.contains(a))
                        .collect();
                    ro_attrs.sort();
                    nodes[idx].read_only_attributes =
                        ro_attrs.into_iter().map(|a| a.to_string()).collect();
                }
            }
            Effect::Update { from, to, .. } => {
                let unchanged_count = from
                    .attributes
                    .iter()
                    .filter(|(k, v)| {
                        !k.starts_with('_')
                            && to
                                .attributes
                                .get(k.as_str())
                                .map(|nv| nv.semantically_equal(v))
                                .unwrap_or(false)
                    })
                    .count();
                nodes[idx].unchanged_count = unchanged_count;
            }
            Effect::Replace {
                from,
                to,
                changed_create_only,
                ..
            } => {
                let changed_set: HashSet<&str> =
                    changed_create_only.iter().map(|s| s.as_str()).collect();
                let unchanged_count = from
                    .attributes
                    .iter()
                    .filter(|(k, v)| {
                        !k.starts_with('_')
                            && !changed_set.contains(k.as_str())
                            && to
                                .attributes
                                .get(k.as_str())
                                .map(|nv| nv.semantically_equal(v))
                                .unwrap_or(false)
                    })
                    .count();
                nodes[idx].unchanged_count = unchanged_count;
            }
            _ => {}
        }
    }
}

fn effect_to_node(effect: &Effect) -> TreeNode {
    match effect {
        Effect::Read { resource } => TreeNode {
            effect_label: format!("{}", resource.id),
            resource_type: resource.id.display_type(),
            name_part: resource.id.name.clone(),
            symbol: "<=".to_string(),
            kind: EffectKind::Read,
            attributes: format_attributes(&resource.attributes),
            changed_attributes: Vec::new(),
            from_attributes: Vec::new(),
            raw_from_attrs: HashMap::new(),
            raw_to_attrs: HashMap::new(),
            default_attributes: Vec::new(),
            read_only_attributes: Vec::new(),
            unchanged_count: 0,

            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Create(resource) => TreeNode {
            effect_label: format!("{}", resource.id),
            resource_type: resource.id.display_type(),
            name_part: resource.id.name.clone(),
            symbol: "+".to_string(),
            kind: EffectKind::Create,
            attributes: format_attributes(&resource.attributes),
            changed_attributes: Vec::new(),
            from_attributes: Vec::new(),
            raw_from_attrs: HashMap::new(),
            raw_to_attrs: HashMap::new(),
            default_attributes: Vec::new(),
            read_only_attributes: Vec::new(),
            unchanged_count: 0,

            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Update {
            id,
            from,
            to,
            changed_attributes,
        } => TreeNode {
            effect_label: format!("{}", id),
            resource_type: id.display_type(),
            name_part: id.name.clone(),
            symbol: "~".to_string(),
            kind: EffectKind::Update,
            attributes: format_attributes(&to.attributes),
            changed_attributes: changed_attributes.clone(),
            from_attributes: format_attributes(&from.attributes),
            raw_from_attrs: from.attributes.clone(),
            raw_to_attrs: to.attributes.clone(),
            default_attributes: Vec::new(),
            read_only_attributes: Vec::new(),
            unchanged_count: 0,

            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Replace {
            id,
            from,
            to,
            lifecycle,
            changed_create_only,
            ..
        } => {
            let symbol = if lifecycle.create_before_destroy {
                "+/-".to_string()
            } else {
                "-/+".to_string()
            };
            TreeNode {
                effect_label: format!("{}", id),
                resource_type: id.display_type(),
                name_part: id.name.clone(),
                symbol,
                kind: EffectKind::Replace,
                attributes: format_attributes(&to.attributes),
                changed_attributes: changed_create_only.clone(),
                from_attributes: format_attributes(&from.attributes),
                raw_from_attrs: from.attributes.clone(),
                raw_to_attrs: to.attributes.clone(),
                default_attributes: Vec::new(),
                read_only_attributes: Vec::new(),
                unchanged_count: 0,

                children: Vec::new(),
                depth: 0,
                parent: None,
            }
        }
        Effect::Delete { id, identifier, .. } => {
            let mut attrs = Vec::new();
            if !identifier.is_empty() {
                attrs.push(("identifier".to_string(), identifier.clone()));
            }
            TreeNode {
                effect_label: format!("{}", id),
                resource_type: id.display_type(),
                name_part: id.name.clone(),
                symbol: "-".to_string(),
                kind: EffectKind::Delete,
                attributes: attrs,
                changed_attributes: Vec::new(),
                from_attributes: Vec::new(),
                raw_from_attrs: HashMap::new(),
                raw_to_attrs: HashMap::new(),
                default_attributes: Vec::new(),
                read_only_attributes: Vec::new(),
                unchanged_count: 0,

                children: Vec::new(),
                depth: 0,
                parent: None,
            }
        }
    }
}

/// Format resource attributes into displayable key-value pairs.
/// Filters out internal attributes (prefixed with "_").
fn format_attributes(attrs: &HashMap<String, Value>) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = attrs
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| (k.clone(), format_value(v)))
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Format a Value for display
pub fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => {
            if carina_core::utils::is_dsl_enum_format(s) {
                s.clone()
            } else {
                format!("\"{}\"", s)
            }
        }
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(map) => {
            let mut entries: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", k, format_value(v)))
                .collect();
            entries.sort();
            format!("{{{}}}", entries.join(", "))
        }
        Value::ResourceRef {
            binding_name,
            attribute_name,
        } => format!("{}.{}", binding_name, attribute_name),
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => format!("{}.{}", name, m),
            None => name.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State};

    #[test]
    fn app_from_empty_plan() {
        let plan = Plan::new();
        let app = App::new(&plan, &HashMap::new());
        assert_eq!(app.nodes.len(), 0);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn app_from_plan_with_effects() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "my-bucket")));
        plan.add(Effect::Delete {
            id: ResourceId::new("s3.bucket", "old-bucket"),
            identifier: "old-bucket-id".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: None,
            dependencies: HashSet::new(),
        });

        let app = App::new(&plan, &HashMap::new());
        assert_eq!(app.nodes.len(), 2);
        assert_eq!(app.nodes[0].symbol, "+");
        assert_eq!(app.nodes[0].kind, EffectKind::Create);
        assert_eq!(app.nodes[1].symbol, "-");
        assert_eq!(app.nodes[1].kind, EffectKind::Delete);
    }

    #[test]
    fn navigation() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        plan.add(Effect::Create(Resource::new("s3.bucket", "b")));
        plan.add(Effect::Create(Resource::new("s3.bucket", "c")));

        let mut app = App::new(&plan, &HashMap::new());
        assert_eq!(app.selected, 0);

        app.move_down();
        assert_eq!(app.selected, 1);

        app.move_down();
        assert_eq!(app.selected, 2);

        // Should not go past end
        app.move_down();
        assert_eq!(app.selected, 2);

        app.move_up();
        assert_eq!(app.selected, 1);

        app.move_up();
        assert_eq!(app.selected, 0);

        // Should not go before start
        app.move_up();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn update_effect_shows_changed_attributes() {
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            id: ResourceId::new("s3.bucket", "my-bucket"),
            from: Box::new(State::existing(
                ResourceId::new("s3.bucket", "my-bucket"),
                [(
                    "versioning".to_string(),
                    Value::String("Disabled".to_string()),
                )]
                .into_iter()
                .collect(),
            )),
            to: Resource::new("s3.bucket", "my-bucket")
                .with_attribute("versioning", Value::String("Enabled".to_string())),
            changed_attributes: vec!["versioning".to_string()],
        });

        let app = App::new(&plan, &HashMap::new());
        assert_eq!(app.nodes[0].kind, EffectKind::Update);
        assert_eq!(app.nodes[0].changed_attributes, vec!["versioning"]);
        assert!(!app.nodes[0].from_attributes.is_empty());
    }

    #[test]
    fn internal_attributes_filtered() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("s3.bucket", "my-bucket")
                .with_attribute("name", Value::String("test".to_string()))
                .with_attribute("_binding", Value::String("my_bucket".to_string()))
                .with_attribute("_module", Value::String("web".to_string())),
        ));

        let app = App::new(&plan, &HashMap::new());
        // Only "name" should appear (not _binding or _module)
        assert_eq!(app.nodes[0].attributes.len(), 1);
        assert_eq!(app.nodes[0].attributes[0].0, "name");
    }

    #[test]
    fn format_value_display() {
        assert_eq!(
            format_value(&Value::String("hello".to_string())),
            "\"hello\""
        );
        assert_eq!(format_value(&Value::Int(42)), "42");
        assert_eq!(format_value(&Value::Bool(true)), "true");
        assert_eq!(
            format_value(&Value::List(vec![Value::Int(1), Value::Int(2)])),
            "[1, 2]"
        );
    }

    #[test]
    fn replace_effect_symbols() {
        let mut plan = Plan::new();
        let from = Box::new(State::existing(
            ResourceId::new("ec2.vpc", "my-vpc"),
            [("cidr".to_string(), Value::String("10.0.0.0/16".to_string()))]
                .into_iter()
                .collect(),
        ));

        // create_before_destroy = true -> "+/-"
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.vpc", "my-vpc"),
            from: from.clone(),
            to: Resource::new("ec2.vpc", "my-vpc"),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["cidr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        // create_before_destroy = false -> "-/+"
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.vpc", "my-vpc2"),
            from,
            to: Resource::new("ec2.vpc", "my-vpc2"),
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["cidr".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let app = App::new(&plan, &HashMap::new());
        assert_eq!(app.nodes[0].symbol, "+/-");
        assert_eq!(app.nodes[1].symbol, "-/+");
    }

    #[test]
    fn tree_structure_with_dependencies() {
        // Create a plan where subnet depends on vpc via ResourceRef
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string()))
                .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "my-subnet")
                .with_attribute("_binding", Value::String("subnet".to_string()))
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                ),
        ));

        let app = App::new(&plan, &HashMap::new());

        // VPC should be root (depth 0) with subnet as child
        assert_eq!(app.nodes[0].depth, 0);
        assert!(app.nodes[0].parent.is_none());
        assert_eq!(app.nodes[0].children, vec![1]);

        // Subnet should be child (depth 1) with VPC as parent
        assert_eq!(app.nodes[1].depth, 1);
        assert_eq!(app.nodes[1].parent, Some(0));
        assert!(app.nodes[1].children.is_empty());
    }

    #[test]
    fn selected_node_returns_correct_node() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("s3.bucket", "my-bucket")
                .with_attribute("name", Value::String("test".to_string())),
        ));

        let app = App::new(&plan, &HashMap::new());
        let node = app.selected_node().unwrap();
        assert_eq!(node.kind, EffectKind::Create);
        // Attributes are always available in the detail panel
        assert!(!node.attributes.is_empty());
    }

    #[test]
    fn toggle_focus_switches_panels() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        let mut app = App::new(&plan, &HashMap::new());

        assert_eq!(app.focused_panel, FocusedPanel::Tree);
        app.toggle_focus();
        assert_eq!(app.focused_panel, FocusedPanel::Detail);
        app.toggle_focus();
        assert_eq!(app.focused_panel, FocusedPanel::Tree);
    }

    #[test]
    fn detail_scroll_up_down() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        let mut app = App::new(&plan, &HashMap::new());

        assert_eq!(app.detail_scroll, 0);
        app.detail_scroll_down();
        assert_eq!(app.detail_scroll, 1);
        app.detail_scroll_down();
        assert_eq!(app.detail_scroll, 2);
        app.detail_scroll_up();
        assert_eq!(app.detail_scroll, 1);
        app.detail_scroll_up();
        assert_eq!(app.detail_scroll, 0);
        // Should not underflow
        app.detail_scroll_up();
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn detail_scroll_resets_on_navigation() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        plan.add(Effect::Create(Resource::new("s3.bucket", "b")));
        let mut app = App::new(&plan, &HashMap::new());

        app.detail_scroll = 5;
        app.move_down();
        assert_eq!(app.detail_scroll, 0);

        app.detail_scroll = 3;
        app.move_up();
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn tree_scroll_cursor_moves_within_visible_area_before_scrolling() {
        // Create a plan with 10 items
        let mut plan = Plan::new();
        for i in 0..10 {
            plan.add(Effect::Create(Resource::new(
                "s3.bucket",
                format!("bucket-{}", i),
            )));
        }
        let mut app = App::new(&plan, &HashMap::new());
        // Simulate a visible area of 5 items
        app.tree_area_height = 5;

        // Move down from 0 to 4: no scrolling needed (items 0-4 fit in view)
        for i in 1..=4 {
            app.move_down();
            assert_eq!(app.selected, i);
            assert_eq!(app.tree_scroll_offset, 0, "should not scroll at item {}", i);
        }

        // Move down to 5: now scroll offset should advance to 1
        app.move_down();
        assert_eq!(app.selected, 5);
        assert_eq!(app.tree_scroll_offset, 1);

        // Move down to 9
        for _ in 6..=9 {
            app.move_down();
        }
        assert_eq!(app.selected, 9);
        assert_eq!(app.tree_scroll_offset, 5); // items 5-9 visible

        // Now move up: cursor moves within visible area without scrolling
        app.move_up(); // selected=8, still in view (5-9)
        assert_eq!(app.selected, 8);
        assert_eq!(app.tree_scroll_offset, 5);

        app.move_up(); // selected=7
        assert_eq!(app.selected, 7);
        assert_eq!(app.tree_scroll_offset, 5);

        app.move_up(); // selected=6
        assert_eq!(app.selected, 6);
        assert_eq!(app.tree_scroll_offset, 5);

        app.move_up(); // selected=5, still at top of view
        assert_eq!(app.selected, 5);
        assert_eq!(app.tree_scroll_offset, 5);

        // Move up past the top of visible area: scroll offset decreases
        app.move_up(); // selected=4, scroll_offset=4
        assert_eq!(app.selected, 4);
        assert_eq!(app.tree_scroll_offset, 4);

        app.move_up(); // selected=3, scroll_offset=3
        assert_eq!(app.selected, 3);
        assert_eq!(app.tree_scroll_offset, 3);
    }

    #[test]
    fn tree_scroll_zero_height_does_not_scroll_on_move_down() {
        // When tree_area_height is 0 (before first render), move_down should not scroll
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        plan.add(Effect::Create(Resource::new("s3.bucket", "b")));
        let mut app = App::new(&plan, &HashMap::new());
        assert_eq!(app.tree_area_height, 0);

        app.move_down();
        assert_eq!(app.selected, 1);
        assert_eq!(app.tree_scroll_offset, 0);
    }
}
