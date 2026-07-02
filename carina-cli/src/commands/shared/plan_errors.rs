use colored::Colorize;

use carina_core::plan::{Plan, PlanError, PlanErrorKind};

use crate::error::AppError;

pub(crate) fn render_plan_errors_and_abort(plan: &Plan) -> Result<(), AppError> {
    let errors = plan.errors();
    if errors.is_empty() {
        return Ok(());
    }

    for err in errors {
        eprintln!("{} {}", "Error:".red().bold(), err);
    }
    Err(AppError::Validation(plan_error_summary(errors)))
}

fn plan_error_summary(errors: &[PlanError]) -> String {
    let mut prevent_destroy_count = 0;
    let mut other_count = 0;

    for err in errors {
        match &err.kind {
            PlanErrorKind::PreventDestroy { .. } => prevent_destroy_count += 1,
            PlanErrorKind::MissingNameAttribute(_)
            | PlanErrorKind::WaitTargetMissing { .. }
            | PlanErrorKind::WaitPredicateInvalid { .. } => other_count += 1,
        }
    }

    let prevent_destroy_summary = || {
        format!(
            "{} resource(s) have prevent_destroy set and cannot be deleted or replaced",
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

    use carina_core::plan::{MissingNameAttributeError, PreventDestroyAction};
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

    fn abort_summary(plan: &Plan) -> String {
        render_plan_errors_and_abort(plan)
            .expect_err("plan errors must abort")
            .to_string()
    }

    #[test]
    fn empty_plan_error_helper_returns_ok() {
        assert!(render_plan_errors_and_abort(&Plan::new()).is_ok());
    }

    #[test]
    fn missing_name_attribute_plan_aborts_with_generic_summary() {
        let plan = plan_with_errors(vec![error(PlanErrorKind::MissingNameAttribute(
            MissingNameAttributeError {
                resource_type: "mock.test.resource".to_string(),
                resource_identity: "example".to_string(),
            },
        ))]);
        let summary = abort_summary(&plan);

        assert_eq!(summary, "plan failed with 1 error(s)");
        assert!(
            !summary.contains("prevent_destroy set"),
            "missing-name failures must not be summarized as prevent_destroy"
        );
    }

    #[test]
    fn prevent_destroy_summary_keeps_existing_text() {
        let plan = plan_with_errors(vec![error(PlanErrorKind::PreventDestroy {
            action: PreventDestroyAction::Delete,
        })]);
        let summary = abort_summary(&plan);

        assert_eq!(
            summary,
            "1 resource(s) have prevent_destroy set and cannot be deleted or replaced"
        );
    }

    #[test]
    fn mixed_error_summary_counts_only_actual_prevent_destroy_errors() {
        let plan = plan_with_errors(vec![
            error(PlanErrorKind::PreventDestroy {
                action: PreventDestroyAction::Delete,
            }),
            error(PlanErrorKind::MissingNameAttribute(
                MissingNameAttributeError {
                    resource_type: "mock.test.resource".to_string(),
                    resource_identity: "example".to_string(),
                },
            )),
        ]);
        let summary = abort_summary(&plan);

        assert_eq!(
            summary,
            "1 resource(s) have prevent_destroy set and cannot be deleted or replaced\n\
             plan failed with 2 error(s)"
        );
    }
}
