use carina_core::effect::Effect;
use carina_core::parser::{DeferredForExpression, ResolvedDeferredForExpressions};
use carina_core::plan::{DeferredSummaryAction, DeferredSummaryEntry, Plan, PlanSummaryPart};

use crate::commands::plan::{ExportChange, ReloadedPlanFile};
use crate::commands::shared::state_writeback::PersistedDirectiveChange;

#[cfg(test)]
pub(crate) fn resolved_deferred_for_for_test(
    expressions: impl IntoIterator<Item = DeferredForExpression>,
) -> ResolvedDeferredForExpressions {
    ResolvedDeferredForExpressions::from_expressions_for_test(expressions)
}

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
    Release,
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
    persisted_directive_changes: Vec<PersistedDirectiveChange>,
    residual_deferred_for: Vec<DeferredForExpression>,
    has_plan_effects: bool,
}

impl PlanChangeSummary {
    pub fn new<S>(
        plan: &Plan<S>,
        export_changes: &[ExportChange],
        residual_deferred_for: &ResolvedDeferredForExpressions,
    ) -> Self {
        Self::from_parts(
            plan,
            export_changes,
            residual_deferred_for.as_slice().to_vec(),
        )
    }

    pub(crate) fn from_reloaded_plan(plan_file: &ReloadedPlanFile) -> Self {
        Self::from_parts(plan_file.plan(), &[], Vec::new())
    }

    fn from_parts<S>(
        plan: &Plan<S>,
        export_changes: &[ExportChange],
        residual_deferred_for: Vec<DeferredForExpression>,
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
            (PlanEffectSummaryKind::Release, plan_summary.release),
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
            persisted_directive_changes: Vec::new(),
            residual_deferred_for,
            has_plan_effects: !plan.effects().is_empty()
                || !plan.ignored_change_summaries().is_empty(),
        }
    }

    pub(crate) fn has_changes(&self) -> bool {
        self.has_non_directive_changes() || !self.persisted_directive_changes.is_empty()
    }

    pub(crate) fn has_non_directive_changes(&self) -> bool {
        self.has_plan_effects
            || !self.export_changes.is_empty()
            || !self.residual_deferred_for.is_empty()
    }

    pub(crate) fn with_persisted_directive_changes(
        mut self,
        changes: Vec<PersistedDirectiveChange>,
    ) -> Self {
        self.persisted_directive_changes = changes;
        self
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

    pub(crate) fn persisted_directive_change_count(&self) -> usize {
        self.persisted_directive_changes.len()
    }

    pub(crate) fn export_changes(&self) -> &[ExportChange] {
        &self.export_changes
    }

    pub(crate) fn persisted_directive_changes(&self) -> &[PersistedDirectiveChange] {
        &self.persisted_directive_changes
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

    use super::{PlanChangeSummary, resolved_deferred_for_for_test};
    use crate::commands::plan::ExportChange;

    fn residual_deferred_for() -> DeferredForExpression {
        DeferredForExpression {
            file: None,
            line: 1,
            header: "for item in upstream.items".to_string(),
            resource_type: "test.Widget".to_string(),
            attributes: Vec::new(),
            target: carina_core::parser::DeferredForTarget::Bound {
                binding: "widgets".to_string(),
            },
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
        let no_deferred = resolved_deferred_for_for_test([]);
        let deferred = resolved_deferred_for_for_test([residual_deferred_for()]);

        assert!(!PlanChangeSummary::new(&plan, &[], &no_deferred).has_changes());
        assert!(
            PlanChangeSummary::new(&plan, std::slice::from_ref(&export_change), &no_deferred,)
                .has_changes()
        );
        assert!(PlanChangeSummary::new(&plan, &[], &deferred).has_changes());
    }

    #[test]
    fn read_effect_uses_the_same_has_changes_decision_as_text_output() {
        let mut plan = Plan::new();
        plan.add(Effect::Read {
            resource: ResolvedDataSource::new(DataSource::new("test.lookup", "lookup")),
        });

        let no_deferred = resolved_deferred_for_for_test([]);
        assert!(PlanChangeSummary::new(&plan, &[], &no_deferred).has_changes());
    }
}
