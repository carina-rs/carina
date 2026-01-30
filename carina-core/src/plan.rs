//! Plan - Collection of Effects
//!
//! A Plan is an ordered list of Effects to be executed.
//! No side effects occur until the Plan is applied.

use std::collections::HashMap;

use crate::effect::Effect;
use crate::module::DependencyGraph;
use crate::resource::Value;

/// Plan containing Effects to be executed
#[derive(Debug, Clone, Default)]
pub struct Plan {
    effects: Vec<Effect>,
}

impl Plan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// Number of mutating Effects
    pub fn mutation_count(&self) -> usize {
        self.effects.iter().filter(|e| e.is_mutating()).count()
    }

    /// Generate a summary of the Plan for display
    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        for effect in &self.effects {
            match effect {
                Effect::Read { .. } => summary.read += 1,
                Effect::Create(_) => summary.create += 1,
                Effect::Update { .. } => summary.update += 1,
                Effect::Delete(_) => summary.delete += 1,
            }
        }
        summary
    }
}

#[derive(Debug, Default)]
pub struct PlanSummary {
    pub read: usize,
    pub create: usize,
    pub update: usize,
    pub delete: usize,
}

impl std::fmt::Display for PlanSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.read > 0 {
            write!(
                f,
                "Plan: {} to read, {} to create, {} to update, {} to delete",
                self.read, self.create, self.update, self.delete
            )
        } else {
            write!(
                f,
                "Plan: {} to create, {} to update, {} to delete",
                self.create, self.update, self.delete
            )
        }
    }
}

/// Source of a resource (root or from a module)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleSource {
    /// Resource defined at the root level
    Root,
    /// Resource from a module instantiation
    Module {
        /// Module name (e.g., "web_tier")
        name: String,
        /// Instance binding name (e.g., "web")
        instance: String,
    },
}

impl ModuleSource {
    /// Create a Root source
    pub fn root() -> Self {
        Self::Root
    }

    /// Create a Module source
    pub fn module(name: impl Into<String>, instance: impl Into<String>) -> Self {
        Self::Module {
            name: name.into(),
            instance: instance.into(),
        }
    }

    /// Check if this is the root source
    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }
}

/// A Plan with module source information
#[derive(Debug, Clone, Default)]
pub struct ModularPlan {
    /// The underlying plan
    pub plan: Plan,
    /// Effect index -> module source mapping
    pub effect_sources: HashMap<usize, ModuleSource>,
    /// Module name -> dependency graph
    pub module_graphs: HashMap<String, DependencyGraph>,
}

impl ModularPlan {
    /// Create a new empty modular plan
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a modular plan from a regular plan with source extraction
    pub fn from_plan(plan: Plan) -> Self {
        let mut modular = Self {
            plan: plan.clone(),
            effect_sources: HashMap::new(),
            module_graphs: HashMap::new(),
        };

        // Extract module sources from effect resources
        for (idx, effect) in plan.effects().iter().enumerate() {
            let source = match effect {
                Effect::Create(r) => Self::extract_source(&r.attributes),
                Effect::Update { to, .. } => Self::extract_source(&to.attributes),
                Effect::Read { resource } => Self::extract_source(&resource.attributes),
                Effect::Delete(_) => ModuleSource::Root,
            };
            modular.effect_sources.insert(idx, source);
        }

        modular
    }

    fn extract_source(attrs: &HashMap<String, Value>) -> ModuleSource {
        let module_name = attrs.get("_module").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });
        let instance_name = attrs.get("_module_instance").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

        match (module_name, instance_name) {
            (Some(name), Some(instance)) => ModuleSource::Module { name, instance },
            _ => ModuleSource::Root,
        }
    }

    /// Get the source for an effect by index
    pub fn source_of(&self, effect_idx: usize) -> &ModuleSource {
        self.effect_sources
            .get(&effect_idx)
            .unwrap_or(&ModuleSource::Root)
    }

    /// Group effects by module source
    pub fn group_by_module(&self) -> HashMap<ModuleSource, Vec<usize>> {
        let mut groups: HashMap<ModuleSource, Vec<usize>> = HashMap::new();

        for (idx, source) in &self.effect_sources {
            groups.entry(source.clone()).or_default().push(*idx);
        }

        // Sort indices within each group
        for indices in groups.values_mut() {
            indices.sort();
        }

        groups
    }

    /// Display effects grouped by module
    pub fn display_by_module(&self) -> String {
        let mut output = String::new();
        let groups = self.group_by_module();

        // Display root effects first
        if let Some(indices) = groups.get(&ModuleSource::Root) {
            output.push_str("Root:\n");
            for idx in indices {
                if let Some(effect) = self.plan.effects().get(*idx) {
                    output.push_str(&format!("  {}\n", format_effect_brief(effect)));
                }
            }
            output.push('\n');
        }

        // Display module effects
        let mut module_sources: Vec<_> = groups.keys().filter(|s| !s.is_root()).cloned().collect();
        module_sources.sort_by(|a, b| match (a, b) {
            (
                ModuleSource::Module {
                    name: n1,
                    instance: i1,
                },
                ModuleSource::Module {
                    name: n2,
                    instance: i2,
                },
            ) => (n1, i1).cmp(&(n2, i2)),
            _ => std::cmp::Ordering::Equal,
        });

        for source in module_sources {
            if let ModuleSource::Module { name, instance } = &source {
                output.push_str(&format!("Module: {} (instance: {})\n", name, instance));

                if let Some(indices) = groups.get(&source) {
                    for idx in indices {
                        if let Some(effect) = self.plan.effects().get(*idx) {
                            output.push_str(&format!("  {}\n", format_effect_brief(effect)));
                        }
                    }
                }
                output.push('\n');
            }
        }

        // Add summary
        let summary = self.plan.summary();
        output.push_str(&format!(
            "Summary: {} to create, {} to update, {} to delete\n",
            summary.create, summary.update, summary.delete
        ));

        output
    }
}

/// Format an effect briefly for display
fn format_effect_brief(effect: &Effect) -> String {
    match effect {
        Effect::Create(r) => format!("+ {}.{}", r.id.resource_type, r.id.name),
        Effect::Update { id, .. } => format!("~ {}.{}", id.resource_type, id.name),
        Effect::Delete(id) => format!("- {}.{}", id.resource_type, id.name),
        Effect::Read { resource } => format!(
            "? {}.{} (data source)",
            resource.id.resource_type, resource.id.name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::Resource;

    #[test]
    fn empty_plan() {
        let plan = Plan::new();
        assert!(plan.is_empty());
        assert_eq!(plan.mutation_count(), 0);
    }

    #[test]
    fn plan_summary() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3_bucket", "a")));
        plan.add(Effect::Create(Resource::new("s3_bucket", "b")));
        plan.add(Effect::Delete(crate::resource::ResourceId::new(
            "s3_bucket",
            "c",
        )));

        let summary = plan.summary();
        assert_eq!(summary.create, 2);
        assert_eq!(summary.delete, 1);
    }

    #[test]
    fn modular_plan_from_plan() {
        let mut plan = Plan::new();

        // Root resource
        plan.add(Effect::Create(Resource::new("vpc", "main")));

        // Module resource
        let mut module_resource = Resource::new("security_group", "web_sg");
        module_resource
            .attributes
            .insert("_module".to_string(), Value::String("web_tier".to_string()));
        module_resource.attributes.insert(
            "_module_instance".to_string(),
            Value::String("web".to_string()),
        );
        plan.add(Effect::Create(module_resource));

        let modular = ModularPlan::from_plan(plan);

        assert_eq!(modular.source_of(0), &ModuleSource::Root);
        assert_eq!(
            modular.source_of(1),
            &ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string()
            }
        );
    }

    #[test]
    fn modular_plan_group_by_module() {
        let mut plan = Plan::new();

        // Two root resources
        plan.add(Effect::Create(Resource::new("vpc", "main")));
        plan.add(Effect::Create(Resource::new("subnet", "public")));

        // Module resource
        let mut module_resource = Resource::new("security_group", "web_sg");
        module_resource
            .attributes
            .insert("_module".to_string(), Value::String("web_tier".to_string()));
        module_resource.attributes.insert(
            "_module_instance".to_string(),
            Value::String("web".to_string()),
        );
        plan.add(Effect::Create(module_resource));

        let modular = ModularPlan::from_plan(plan);
        let groups = modular.group_by_module();

        assert_eq!(groups.get(&ModuleSource::Root).unwrap().len(), 2);
        assert_eq!(
            groups
                .get(&ModuleSource::module("web_tier", "web"))
                .unwrap()
                .len(),
            1
        );
    }
}
