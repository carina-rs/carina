//! Application state for the TUI plan viewer

use std::collections::{HashMap, HashSet, VecDeque};

use carina_core::deps::get_resource_dependencies;
use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::resource::Value;
use ratatui::widgets::ListState;

/// A node in the tree view representing one effect
#[derive(Debug)]
pub struct TreeNode {
    /// Effect type label for display
    pub effect_label: String,
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
    /// Whether this node is expanded
    pub expanded: bool,
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

/// Application state
pub struct App {
    /// Tree nodes (one per effect)
    pub nodes: Vec<TreeNode>,
    /// Currently selected index in the visible list
    pub selected: usize,
    /// List state for ratatui scrolling
    pub list_state: ListState,
    /// Plan summary for display
    pub summary: String,
}

impl App {
    pub fn new(plan: &Plan) -> Self {
        let mut nodes: Vec<TreeNode> = plan.effects().iter().map(effect_to_node).collect();
        let summary = format!("{}", plan.summary());

        // Build tree structure from dependency analysis
        build_tree_structure(plan, &mut nodes);

        let mut list_state = ListState::default();
        if !nodes.is_empty() {
            list_state.select(Some(0));
        }
        App {
            nodes,
            selected: 0,
            list_state,
            summary,
        }
    }

    /// Returns indices of visible nodes (roots + expanded descendants).
    pub fn visible_nodes(&self) -> Vec<usize> {
        let mut visible = Vec::new();
        for (idx, node) in self.nodes.iter().enumerate() {
            if node.parent.is_none() {
                self.collect_visible(idx, &mut visible);
            }
        }
        visible
    }

    fn collect_visible(&self, idx: usize, visible: &mut Vec<usize>) {
        visible.push(idx);
        if self.nodes[idx].expanded {
            for &child in &self.nodes[idx].children {
                self.collect_visible(child, visible);
            }
        }
    }

    /// Number of visible items
    pub fn visible_count(&self) -> usize {
        self.visible_nodes().len()
    }

    pub fn move_up(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        // Find current position in visible list
        if let Some(pos) = visible
            .iter()
            .position(|&idx| idx == self.selected)
            .filter(|&pos| pos > 0)
        {
            self.selected = visible[pos - 1];
            // Update list_state to match position in visible list
            self.list_state.select(Some(pos - 1));
        }
    }

    pub fn move_down(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        if let Some(pos) = visible
            .iter()
            .position(|&idx| idx == self.selected)
            .filter(|&pos| pos < visible.len() - 1)
        {
            self.selected = visible[pos + 1];
            self.list_state.select(Some(pos + 1));
        }
    }

    pub fn expand(&mut self) {
        if let Some(node) = self.nodes.get_mut(self.selected)
            && !node.children.is_empty()
        {
            node.expanded = true;
        }
    }

    pub fn collapse(&mut self) {
        if let Some(node) = self.nodes.get(self.selected) {
            if node.expanded {
                // Collapse this node
                self.nodes[self.selected].expanded = false;
            } else if let Some(parent_idx) = node.parent {
                // Navigate to parent and collapse it
                self.nodes[parent_idx].expanded = false;
                self.selected = parent_idx;
                let visible = self.visible_nodes();
                if let Some(pos) = visible.iter().position(|&idx| idx == parent_idx) {
                    self.list_state.select(Some(pos));
                }
            }
        }
    }

    /// Toggle expand/collapse on the selected node
    pub fn toggle(&mut self) {
        if let Some(node) = self.nodes.get(self.selected)
            && !node.children.is_empty()
        {
            let new_state = !node.expanded;
            self.nodes[self.selected].expanded = new_state;
        }
    }

    /// Get the currently selected node, if any
    pub fn selected_node(&self) -> Option<&TreeNode> {
        self.nodes.get(self.selected)
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
            Effect::Delete { .. } => (None, HashSet::new()),
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

fn effect_to_node(effect: &Effect) -> TreeNode {
    match effect {
        Effect::Read { resource } => TreeNode {
            effect_label: format!("{}", resource.id),
            symbol: "<=".to_string(),
            kind: EffectKind::Read,
            attributes: format_attributes(&resource.attributes),
            changed_attributes: Vec::new(),
            from_attributes: Vec::new(),
            expanded: false,
            children: Vec::new(),
            depth: 0,
            parent: None,
        },
        Effect::Create(resource) => TreeNode {
            effect_label: format!("{}", resource.id),
            symbol: "+".to_string(),
            kind: EffectKind::Create,
            attributes: format_attributes(&resource.attributes),
            changed_attributes: Vec::new(),
            from_attributes: Vec::new(),
            expanded: false,
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
            symbol: "~".to_string(),
            kind: EffectKind::Update,
            attributes: format_attributes(&to.attributes),
            changed_attributes: changed_attributes.clone(),
            from_attributes: format_attributes(&from.attributes),
            expanded: false,
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
                symbol,
                kind: EffectKind::Replace,
                attributes: format_attributes(&to.attributes),
                changed_attributes: changed_create_only.clone(),
                from_attributes: format_attributes(&from.attributes),
                expanded: false,
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
                symbol: "-".to_string(),
                kind: EffectKind::Delete,
                attributes: attrs,
                changed_attributes: Vec::new(),
                from_attributes: Vec::new(),
                expanded: false,
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
fn format_value(value: &Value) -> String {
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
        let app = App::new(&plan);
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
        });

        let app = App::new(&plan);
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

        let mut app = App::new(&plan);
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
    fn expand_collapse() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("s3.bucket", "my-bucket")
                .with_attribute("name", Value::String("test".to_string())),
        ));

        let mut app = App::new(&plan);
        assert!(!app.nodes[0].expanded);

        // Expand on a leaf node should be a no-op
        app.expand();
        assert!(!app.nodes[0].expanded);

        // But collapse on a root leaf should also be a no-op
        app.collapse();
        assert!(!app.nodes[0].expanded);
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

        let app = App::new(&plan);
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

        let app = App::new(&plan);
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
        });

        let app = App::new(&plan);
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

        let app = App::new(&plan);

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
    fn visible_nodes_with_collapse() {
        // VPC (root) -> Subnet (child)
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string())),
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

        let mut app = App::new(&plan);

        // Initially collapsed: only root visible
        assert_eq!(app.visible_nodes(), vec![0]);
        assert_eq!(app.visible_count(), 1);

        // Expand root: both visible
        app.expand();
        assert_eq!(app.visible_nodes(), vec![0, 1]);
        assert_eq!(app.visible_count(), 2);

        // Collapse root: only root visible again
        app.collapse();
        assert_eq!(app.visible_nodes(), vec![0]);
        assert_eq!(app.visible_count(), 1);
    }

    #[test]
    fn navigation_skips_collapsed_children() {
        // VPC (root) -> Subnet (child), plus another root S3 bucket
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string())),
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
        plan.add(Effect::Create(
            Resource::new("s3.bucket", "my-bucket")
                .with_attribute("_binding", Value::String("bucket".to_string())),
        ));

        let mut app = App::new(&plan);

        // VPC is collapsed, so visible = [vpc_idx, bucket_idx]
        // The exact indices depend on sort order. Let's check.
        let visible = app.visible_nodes();
        assert_eq!(visible.len(), 2); // VPC root + S3 root, subnet hidden

        // Navigate down from first to second root
        app.move_down();
        let second_root = visible[1];
        assert_eq!(app.selected, second_root);

        // Navigate back up
        app.move_up();
        assert_eq!(app.selected, visible[0]);
    }

    #[test]
    fn leaf_node_has_no_expand_marker() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "my-bucket")));

        let app = App::new(&plan);
        // Leaf node: no children
        assert!(app.nodes[0].children.is_empty());
    }

    #[test]
    fn collapse_navigates_to_parent() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("_binding", Value::String("vpc".to_string())),
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

        let mut app = App::new(&plan);

        // Expand VPC to show subnet
        app.expand();
        assert_eq!(app.visible_nodes(), vec![0, 1]);

        // Navigate to subnet
        app.move_down();
        assert_eq!(app.selected, 1);

        // Collapse on a leaf with parent -> navigates to parent and collapses it
        app.collapse();
        assert_eq!(app.selected, 0);
        assert!(!app.nodes[0].expanded);
        assert_eq!(app.visible_nodes(), vec![0]);
    }
}
