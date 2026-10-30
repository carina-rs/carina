use carina_core::effect::Effect;
use carina_core::parser::DeferredForExpression;
use carina_core::plan::{DeferredSummaryAction, DeferredSummaryEntry, Plan, PlanSummaryPart};

use crate::commands::plan::ExportChange;

/// Effect categories shared by plan text and plan-image summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum PlanEffectSummaryKind {
    Read,
    Create,
    ResumeCreate,
    Update,
    Replace,
    Delete,
    Import,
    Remove,
    Move,
    Wait,
    DeferredCreate,
    DeferredReplace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlanEffectSummaryPart {
    pub kind: PlanEffectSummaryKind,
    pub count: usize,
}

/// The shared display summary used by text and image plan output.
///
/// This deliberately does not define `--detailed-exitcode` semantics. That
/// contract is mutation-only, while display output also reports reads, waits,
/// export changes, and residual deferred loops.
#[derive(Clone)]
pub struct PlanChangeSummary {
    effect_parts: Vec<PlanEffectSummaryPart>,
    text_parts: Vec<PlanSummaryPart>,
    deferred_effects: Vec<DeferredSummaryEntry>,
    export_changes: Vec<ExportChange>,
    residual_deferred_for: Vec<DeferredForExpression>,
    has_plan_effects: bool,
}

impl PlanChangeSummary {
    pub fn new<S>(
        plan: &Plan<S>,
        export_changes: &[ExportChange],
        residual_deferred_for: &[DeferredForExpression],
    ) -> Self {
        let plan_summary = plan.summary();
        let resume_create = plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::ResumeCreate { .. }))
            .count();
        let deferred_create = plan_summary
            .deferred
            .iter()
            .filter(|entry| entry.action == DeferredSummaryAction::Add)
            .count();
        let deferred_replace = plan_summary
            .deferred
            .iter()
            .filter(|entry| entry.action == DeferredSummaryAction::Replace)
            .count();

        let counts = [
            (PlanEffectSummaryKind::Read, plan_summary.read),
            (
                PlanEffectSummaryKind::Create,
                plan_summary.create - resume_create,
            ),
            (PlanEffectSummaryKind::ResumeCreate, resume_create),
            (PlanEffectSummaryKind::Update, plan_summary.update),
            (PlanEffectSummaryKind::Replace, plan_summary.replace),
            (PlanEffectSummaryKind::Delete, plan_summary.delete),
            (PlanEffectSummaryKind::Import, plan_summary.import),
            (PlanEffectSummaryKind::Remove, plan_summary.remove),
            (PlanEffectSummaryKind::Move, plan_summary.moved),
            (PlanEffectSummaryKind::Wait, plan_summary.wait),
            (PlanEffectSummaryKind::DeferredCreate, deferred_create),
            (PlanEffectSummaryKind::DeferredReplace, deferred_replace),
        ];
        let effect_parts = counts
            .into_iter()
            .filter_map(|(kind, count)| {
                (count > 0).then_some(PlanEffectSummaryPart { kind, count })
            })
            .collect();
        let text_parts = plan_summary.parts();

        Self {
            effect_parts,
            text_parts,
            deferred_effects: plan_summary.deferred,
            export_changes: export_changes.to_vec(),
            residual_deferred_for: residual_deferred_for.to_vec(),
            has_plan_effects: !plan.effects().is_empty()
                || !plan.ignored_change_summaries().is_empty(),
        }
    }

    pub(crate) fn has_changes(&self) -> bool {
        self.has_plan_effects
            || !self.export_changes.is_empty()
            || !self.residual_deferred_for.is_empty()
    }

    pub(crate) fn effect_parts(&self) -> &[PlanEffectSummaryPart] {
        &self.effect_parts
    }

    pub(crate) fn text_parts(&self) -> &[PlanSummaryPart] {
        &self.text_parts
    }

    pub(crate) fn deferred_effects(&self) -> &[DeferredSummaryEntry] {
        &self.deferred_effects
    }

    pub(crate) fn export_change_count(&self) -> usize {
        self.export_changes.len()
    }

    pub(crate) fn residual_deferred_for_count(&self) -> usize {
        self.residual_deferred_for.len()
    }

    pub(crate) fn export_changes(&self) -> &[ExportChange] {
        &self.export_changes
    }

    pub(crate) fn residual_deferred_for(&self) -> &[DeferredForExpression] {
        &self.residual_deferred_for
    }
}

#[cfg(test)]
mod tests {
    use carina_core::effect::Effect;
    use carina_core::parser::{DeferredForExpression, ForBinding};
    use carina_core::plan::Plan;
    use carina_core::resource::{ConcreteValue, DataSource, ResolvedDataSource, Resource, Value};

    use super::PlanChangeSummary;
    use crate::commands::plan::ExportChange;

    fn residual_deferred_for() -> DeferredForExpression {
        DeferredForExpression {
            file: None,
            line: 1,
            header: "for item in upstream.items".to_string(),
            resource_type: "test.Widget".to_string(),
            attributes: Vec::new(),
            binding_name: "widgets".to_string(),
            iterable: Value::resource_ref("upstream", "items", Vec::new()),
            binding: ForBinding::Simple("item".to_string()),
            template_resource: Resource::new("test.Widget", "widgets"),
        }
    }

    #[test]
    fn non_resource_changes_share_the_has_changes_decision() {
        let plan = Plan::new();
        let export_change = ExportChange::Added {
            name: "region".to_string(),
            type_expr: None,
            new_value: Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        };
        let deferred = residual_deferred_for();

        assert!(!PlanChangeSummary::new(&plan, &[], &[]).has_changes());
        assert!(
            PlanChangeSummary::new(&plan, std::slice::from_ref(&export_change), &[]).has_changes()
        );
        assert!(PlanChangeSummary::new(&plan, &[], std::slice::from_ref(&deferred)).has_changes());
    }

    #[test]
    fn read_effect_uses_the_same_has_changes_decision_as_text_output() {
        let mut plan = Plan::new();
        plan.add(Effect::Read {
            resource: ResolvedDataSource::new(DataSource::new("test.lookup", "lookup")),
        });

        assert!(PlanChangeSummary::new(&plan, &[], &[]).has_changes());
    }
}
