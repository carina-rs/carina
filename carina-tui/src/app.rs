//! Application state for the TUI plan viewer

use std::collections::HashMap;

use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::resource::Value;

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
    /// Plan summary for display
    pub summary: String,
}

impl App {
    pub fn new(plan: &Plan) -> Self {
        let nodes: Vec<TreeNode> = plan.effects().iter().map(effect_to_node).collect();
        let summary = format!("{}", plan.summary());
        App {
            nodes,
            selected: 0,
            summary,
        }
    }

    /// Number of visible items (all top-level nodes are always visible)
    pub fn visible_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        let count = self.visible_count();
        if count > 0 && self.selected < count - 1 {
            self.selected += 1;
        }
    }

    pub fn expand(&mut self) {
        if let Some(node) = self.nodes.get_mut(self.selected) {
            node.expanded = true;
        }
    }

    pub fn collapse(&mut self) {
        if let Some(node) = self.nodes.get_mut(self.selected) {
            node.expanded = false;
        }
    }

    /// Get the currently selected node, if any
    pub fn selected_node(&self) -> Option<&TreeNode> {
        self.nodes.get(self.selected)
    }
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
        },
        Effect::Create(resource) => TreeNode {
            effect_label: format!("{}", resource.id),
            symbol: "+".to_string(),
            kind: EffectKind::Create,
            attributes: format_attributes(&resource.attributes),
            changed_attributes: Vec::new(),
            from_attributes: Vec::new(),
            expanded: false,
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
        Value::String(s) => format!("\"{}\"", s),
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

        app.expand();
        assert!(app.nodes[0].expanded);

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
}
