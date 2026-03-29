//! Plan - Collection of Effects
//!
//! A Plan is an ordered list of Effects to be executed.
//! No side effects occur until the Plan is applied.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::effect::Effect;
use crate::module::DependencyGraph;
pub use crate::resource::ModuleSource;

/// Plan containing Effects to be executed
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Remove effects that don't satisfy the predicate
    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&Effect) -> bool,
    {
        self.effects.retain(f);
    }

    /// Set cascading updates on Replace effects that match the given replaced bindings.
    pub fn set_cascading_updates(
        &mut self,
        replaced_bindings: &std::collections::HashSet<String>,
        updates_by_binding: &std::collections::HashMap<String, Vec<crate::effect::CascadingUpdate>>,
    ) {
        for effect in &mut self.effects {
            if let Effect::Replace {
                to,
                cascading_updates,
                ..
            } = effect
            {
                let binding = to.binding.clone();
                if let Some(binding) = binding
                    && replaced_bindings.contains(&binding)
                    && let Some(updates) = updates_by_binding.get(&binding)
                {
                    *cascading_updates = updates.clone();
                }
            }
        }
    }

    /// Merge cascade-triggered create-only attributes into existing effects.
    ///
    /// For a resource already in the plan:
    /// - If it is a Replace, add the new create-only attrs to `changed_create_only`
    /// - If it is an Update, upgrade it to a Replace with the cascade attrs as `changed_create_only`
    /// - Other effect types (Create, Delete, Read) are left unchanged
    pub fn merge_cascade_create_only(
        &mut self,
        resource_id: &crate::resource::ResourceId,
        cascade_attrs: Vec<String>,
        lifecycle: crate::resource::LifecycleConfig,
        ref_hints: Vec<(String, String)>,
    ) {
        for effect in &mut self.effects {
            match effect {
                Effect::Replace {
                    id,
                    changed_create_only,
                    cascade_ref_hints,
                    ..
                } if id == resource_id => {
                    for attr in &cascade_attrs {
                        if !changed_create_only.contains(attr) {
                            changed_create_only.push(attr.clone());
                        }
                    }
                    for hint in &ref_hints {
                        if !cascade_ref_hints.contains(hint) {
                            cascade_ref_hints.push(hint.clone());
                        }
                    }
                    return;
                }
                Effect::Update { id, .. } if id == resource_id => {
                    // Take ownership of the Update fields and upgrade to Replace
                    let old = std::mem::replace(
                        effect,
                        Effect::Create(crate::resource::Resource::new("", "")),
                    );
                    if let Effect::Update { id, from, to, .. } = old {
                        *effect = Effect::Replace {
                            id,
                            from,
                            to,
                            lifecycle,
                            changed_create_only: cascade_attrs,
                            cascading_updates: vec![],
                            temporary_name: None,
                            cascade_ref_hints: ref_hints,
                        };
                    }
                    return;
                }
                _ => {}
            }
        }
    }

    /// Promote a Replace effect to create_before_destroy.
    ///
    /// This is used by auto-detection: when a resource being replaced is
    /// referenced by other resources, it should use create_before_destroy
    /// to avoid breaking dependents during replacement.
    pub fn promote_to_create_before_destroy(&mut self, resource_id: &crate::resource::ResourceId) {
        for effect in &mut self.effects {
            if let Effect::Replace { id, lifecycle, .. } = effect
                && id == resource_id
            {
                lifecycle.create_before_destroy = true;
                return;
            }
        }
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
                Effect::Replace {
                    cascading_updates, ..
                } => {
                    summary.replace += 1;
                    summary.update += cascading_updates.len();
                }
                Effect::Delete { .. } => summary.delete += 1,
                Effect::Import { .. } => summary.import += 1,
                Effect::Remove { .. } => summary.remove += 1,
                Effect::Move { .. } => summary.moved += 1,
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
    pub replace: usize,
    pub delete: usize,
    pub import: usize,
    pub remove: usize,
    pub moved: usize,
}

impl std::fmt::Display for PlanSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if self.read > 0 {
            parts.push(format!("{} to read", self.read));
        }
        if self.import > 0 {
            parts.push(format!("{} to import", self.import));
        }
        parts.push(format!("{} to create", self.create));
        parts.push(format!("{} to update", self.update));
        if self.replace > 0 {
            parts.push(format!("{} to replace", self.replace));
        }
        parts.push(format!("{} to delete", self.delete));
        if self.remove > 0 {
            parts.push(format!("{} to remove from state", self.remove));
        }
        if self.moved > 0 {
            parts.push(format!("{} to move", self.moved));
        }
        write!(f, "Plan: {}", parts.join(", "))
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
                Effect::Create(r) => Self::extract_source(r),
                Effect::Update { to, .. } => Self::extract_source(to),
                Effect::Replace { to, .. } => Self::extract_source(to),
                Effect::Read { resource } => Self::extract_source(resource),
                Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. } => ModuleSource::Root,
            };
            modular.effect_sources.insert(idx, source);
        }

        modular
    }

    fn extract_source(resource: &crate::resource::Resource) -> ModuleSource {
        resource.module_source.clone().unwrap_or(ModuleSource::Root)
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
        if summary.replace > 0 {
            output.push_str(&format!(
                "Summary: {} to create, {} to update, {} to replace, {} to delete\n",
                summary.create, summary.update, summary.replace, summary.delete
            ));
        } else {
            output.push_str(&format!(
                "Summary: {} to create, {} to update, {} to delete\n",
                summary.create, summary.update, summary.delete
            ));
        }

        output
    }
}

/// Format an effect briefly for display
fn format_effect_brief(effect: &Effect) -> String {
    match effect {
        Effect::Create(r) => format!("+ {}", r.id),
        Effect::Update { id, .. } => format!("~ {}", id),
        Effect::Replace { id, lifecycle, .. } => {
            if lifecycle.create_before_destroy {
                format!("+/- {}", id)
            } else {
                format!("-/+ {}", id)
            }
        }
        Effect::Delete { id, .. } => format!("- {}", id),
        Effect::Read { resource } => format!("<= {} (data source)", resource.id),
        Effect::Import { id, identifier } => format!("<- {} (import: {})", id, identifier),
        Effect::Remove { id } => format!("x {}", id),
        Effect::Move { from, to } => format!("-> {} (from: {})", to, from),
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
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        plan.add(Effect::Create(Resource::new("s3.bucket", "b")));
        plan.add(Effect::Delete {
            id: crate::resource::ResourceId::new("s3.bucket", "c"),
            identifier: String::new(),
            lifecycle: crate::resource::LifecycleConfig::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
        });

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
        let module_resource =
            Resource::new("security_group", "web_sg").with_module_source(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            });
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
        let module_resource =
            Resource::new("security_group", "web_sg").with_module_source(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            });
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

    #[test]
    fn plan_summary_counts_cascading_updates() {
        use crate::effect::CascadingUpdate;
        use crate::resource::State;
        use crate::resource::{LifecycleConfig, ResourceId};

        let mut plan = Plan::new();

        // A Replace effect with one cascading update
        let from = State::not_found(ResourceId::new("ec2.vpc", "vpc")).with_identifier("vpc-123");
        let to = Resource::new("ec2.vpc", "vpc");
        let cascading = CascadingUpdate {
            id: ResourceId::new("ec2.subnet", "subnet"),
            from: Box::new(
                State::not_found(ResourceId::new("ec2.subnet", "subnet"))
                    .with_identifier("subnet-123"),
            ),
            to: Resource::new("ec2.subnet", "subnet"),
        };
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.vpc", "vpc"),
            from: Box::new(from),
            to,
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![cascading],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let summary = plan.summary();
        assert_eq!(summary.replace, 1);
        assert_eq!(summary.update, 1, "cascading updates should be counted");
        assert_eq!(summary.create, 0);
        assert_eq!(summary.delete, 0);
    }

    #[test]
    fn plan_summary_display_includes_cascading_updates() {
        use crate::effect::CascadingUpdate;
        use crate::resource::State;
        use crate::resource::{LifecycleConfig, ResourceId};

        let mut plan = Plan::new();

        let from = State::not_found(ResourceId::new("ec2.vpc", "vpc")).with_identifier("vpc-123");
        let to = Resource::new("ec2.vpc", "vpc");
        let cascading = CascadingUpdate {
            id: ResourceId::new("ec2.subnet", "subnet"),
            from: Box::new(
                State::not_found(ResourceId::new("ec2.subnet", "subnet"))
                    .with_identifier("subnet-123"),
            ),
            to: Resource::new("ec2.subnet", "subnet"),
        };
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.vpc", "vpc"),
            from: Box::new(from),
            to,
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![cascading],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let display = format!("{}", plan.summary());
        assert!(
            display.contains("1 to update"),
            "display should show cascading updates: {}",
            display
        );
        assert!(
            display.contains("1 to replace"),
            "display should show replace: {}",
            display
        );
    }

    #[test]
    fn plan_serde_round_trip() {
        use crate::resource::ResourceId;

        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "a")));
        plan.add(Effect::Delete {
            id: ResourceId::new("s3.bucket", "b"),
            identifier: "b-id".to_string(),
            lifecycle: crate::resource::LifecycleConfig::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
        });

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.effects().len(), deserialized.effects().len());
        assert_eq!(plan.effects(), deserialized.effects());
    }
}
