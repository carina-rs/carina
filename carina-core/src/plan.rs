//! Plan - Collection of Effects
//!
//! A Plan is an ordered list of Effects to be executed.
//! No side effects occur until the Plan is applied.

use crate::effect::Effect;

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
                Effect::Read(_) => summary.read += 1,
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
        write!(
            f,
            "Plan: {} to create, {} to update, {} to delete",
            self.create, self.update, self.delete
        )
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
}
