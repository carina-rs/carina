use crate::output_style::Colorize;
use std::path::Path;

use carina_core::plan::{Plan, PlanError, PlanErrorKind};

use crate::error::AppError;

pub(crate) fn render_plan_errors_and_abort<S>(
    color_mode: crate::output_style::ColorMode,
    plan: &Plan<S>,
    project_dir: &Path,
) -> Result<(), AppError> {
    let errors = plan.errors();
    if errors.is_empty() {
        return Ok(());
    }

    Err(render_plan_errors(color_mode, errors, project_dir))
}

fn render_plan_errors(
    color_mode: crate::output_style::ColorMode,
    errors: &[PlanError],
    project_dir: &Path,
) -> AppError {
    for err in errors {
        eprintln!(
            "{} {}",
            "Error:".red(color_mode).bold(color_mode),
            err.display_with_project_dir(project_dir)
        );
    }
    AppError::Validation(plan_error_summary(errors))
}

impl<R> crate::wiring::PostExpansionDiagnostics<R> {
    /// Render this diagnostic outcome in plan-error order and return the
    /// command error that must abort apply. There is deliberately no success
    /// branch: receiving this type already selected the diagnostic outcome.
    pub(crate) fn render_and_abort(
        &self,
        color_mode: crate::output_style::ColorMode,
        project_dir: &Path,
    ) -> AppError {
        render_plan_errors(color_mode, self.errors().as_slice(), project_dir)
    }
}

fn plan_error_summary(errors: &[PlanError]) -> String {
    let mut prevent_destroy_count = 0;
    let mut other_count = 0;

    for err in errors {
        match &err.kind {
            PlanErrorKind::PreventDestroy { .. } => prevent_destroy_count += 1,
            PlanErrorKind::SchemaNotRegistered(_)
            | PlanErrorKind::ReplacementCannotCoexist(_)
            | PlanErrorKind::CreateBeforeDestroyRequiredByDependent { .. }
            | PlanErrorKind::MissingProviderIdentifier
            | PlanErrorKind::InvalidImportProviderIdentifier
            | PlanErrorKind::MissingDeposedProviderIdentifier { .. }
            | PlanErrorKind::WaitTargetMissing { .. }
            | PlanErrorKind::WaitTargetIsDataSource { .. }
            | PlanErrorKind::WaitPredicateInvalid { .. }
            | PlanErrorKind::ResolvedValueConstraint { .. }
            | PlanErrorKind::IgnoredWriteOnlyValueUnavailable { .. }
            | PlanErrorKind::CreateInFlightOrphan
            | PlanErrorKind::DestroyCreateInFlight
            | PlanErrorKind::CreateOutcomeUnknown { .. }
            | PlanErrorKind::ResumeCreateRoutingMismatch { .. }
            | PlanErrorKind::ModuleConstraint(_) => other_count += 1,
        }
    }

    let prevent_destroy_summary = || {
        format!(
            "{} resource(s) are protected from destruction and cannot be deleted or replaced",
            prevent_destroy_count
        )
    };
    let generic_summary = || format!("plan failed with {} error(s)", errors.len());

    match (prevent_destroy_count, other_count) {
        (0, _) => generic_summary(),
        (_, 0) => prevent_destroy_summary(),
        (_, _) => format!("{}\n{}", prevent_destroy_summary(), generic_summary()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use carina_core::plan::{
        PreventDestroyAction, PreventDestroySource, ReplacementCannotCoexistError,
        SchemaNotRegisteredError,
    };
    use carina_core::resource::ResourceId;

    fn error(kind: PlanErrorKind) -> PlanError {
        PlanError {
            resource_id: ResourceId::with_identity("mock.test.resource", "example"),
            kind,
        }
    }

    fn plan_with_errors(errors: Vec<PlanError>) -> Plan {
        let mut plan = Plan::new();
        for error in errors {
            plan.add_error(error);
        }
        plan
    }

    fn abort_summary(color_mode: crate::output_style::ColorMode, plan: &Plan) -> String {
        render_plan_errors_and_abort(color_mode, plan, Path::new("."))
            .expect_err("plan errors must abort")
            .to_string()
    }

    #[test]
    fn empty_plan_error_helper_returns_ok() {
        let color_mode = crate::output_style::ColorMode::Plain;
        assert!(render_plan_errors_and_abort(color_mode, &Plan::new(), Path::new(".")).is_ok());
    }

    #[test]
    fn schema_not_registered_plan_aborts_with_generic_summary() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let plan = plan_with_errors(vec![error(PlanErrorKind::SchemaNotRegistered(
            SchemaNotRegisteredError {
                resource_type: "mock.test.resource".to_string(),
                resource_identity: "example".to_string(),
            },
        ))]);
        let summary = abort_summary(color_mode, &plan);

        assert_eq!(summary, "plan failed with 1 error(s)");
        assert!(
            !summary.contains("prevent_destroy set"),
            "schema registration failures must not be summarized as prevent_destroy"
        );
    }

    #[test]
    fn replacement_cannot_coexist_plan_aborts_with_generic_summary() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let plan = plan_with_errors(vec![error(PlanErrorKind::ReplacementCannotCoexist(
            ReplacementCannotCoexistError {
                resource_type: "mock.test.resource".to_string(),
                resource_identity: "example".to_string(),
            },
        ))]);
        let summary = abort_summary(color_mode, &plan);

        assert_eq!(summary, "plan failed with 1 error(s)");
        assert!(
            !summary.contains("prevent_destroy set"),
            "conflicting create_before_destroy failures must not be summarized as prevent_destroy"
        );
    }

    #[test]
    fn prevent_destroy_summary_describes_effective_protection() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let plan = plan_with_errors(vec![error(PlanErrorKind::PreventDestroy {
            action: PreventDestroyAction::Delete,
            source: PreventDestroySource::ExplicitDirective,
        })]);
        let summary = abort_summary(color_mode, &plan);

        assert_eq!(
            summary,
            "1 resource(s) are protected from destruction and cannot be deleted or replaced"
        );
    }

    #[test]
    fn mixed_error_summary_counts_only_actual_prevent_destroy_errors() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let plan = plan_with_errors(vec![
            error(PlanErrorKind::PreventDestroy {
                action: PreventDestroyAction::Delete,
                source: PreventDestroySource::SchemaDefault,
            }),
            error(PlanErrorKind::SchemaNotRegistered(
                SchemaNotRegisteredError {
                    resource_type: "mock.test.resource".to_string(),
                    resource_identity: "example".to_string(),
                },
            )),
        ]);
        let summary = abort_summary(color_mode, &plan);

        assert_eq!(
            summary,
            "1 resource(s) are protected from destruction and cannot be deleted or replaced\n\
             plan failed with 2 error(s)"
        );
    }
}
