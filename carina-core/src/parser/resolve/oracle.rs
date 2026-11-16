//! Saved-input replay and comparison only; all resolution remains in the parent body.
use super::*;
use crate::test_utils::resolver_kind::{self as observation, Counts};
use std::collections::VecDeque;

struct ExportView<'a, E> {
    original: &'a E,
    value: Option<S<RelativeBindingName>>,
    type_expr: Option<super::super::TypeExpr>,
    judgement:
        Option<Result<super::super::retained::JudgedExpr, super::super::retained::MissingEvidence>>,
}
impl<'a, E: ExportParamLike> ExportView<'a, E> {
    fn new(original: &'a E) -> Self {
        Self {
            original,
            value: original.value().cloned(),
            type_expr: original.type_expr_opt().cloned(),
            judgement: None,
        }
    }
}
impl<E: ExportParamLike> ExportParamLike for ExportView<'_, E> {
    fn name(&self) -> &str {
        self.original.name()
    }
    fn value(&self) -> Option<&S<RelativeBindingName>> {
        self.value.as_ref()
    }
    fn handle(&self) -> Option<&crate::eval_value::UnexpandedResourceHandle<RelativeBindingName>> {
        self.original.handle()
    }
    fn source_file(&self) -> &std::path::Path {
        self.original.source_file()
    }
    fn source_span(&self) -> super::super::SourceSpan {
        self.original.source_span()
    }
    fn type_check_location(&self) -> &super::super::DeclarationLocation {
        self.original.type_check_location()
    }
    fn replace_value(&mut self, value: S<RelativeBindingName>) {
        self.value = Some(value);
    }
    fn type_expr_opt(&self) -> Option<&super::super::TypeExpr> {
        self.type_expr.as_ref()
    }
    fn type_expr_opt_mut(&mut self) -> Option<&mut super::super::TypeExpr> {
        self.type_expr.as_mut()
    }
    fn inference_failed(&self) -> bool {
        self.original.inference_failed()
    }
    fn value_location(&self) -> Option<&super::super::DeclarationLocation> {
        self.original.value_location()
    }
    fn judgement(
        &self,
    ) -> Result<&super::super::retained::JudgedExpr, &super::super::retained::MissingEvidence> {
        match &self.judgement {
            Some(judgement) => judgement.as_ref(),
            None => self.original.judgement(),
        }
    }
    fn attach_judgement(
        &mut self,
        report: &super::super::retained::ExpressionReport<RelativeBindingName>,
    ) {
        self.judgement = Some(
            self.value_location()
                .ok_or_else(|| {
                    super::super::retained::MissingEvidence::new("declaration value location")
                })
                .and_then(|location| report.expression(location))
                .cloned(),
        );
    }
    fn source_location(&self) -> Option<(&std::path::Path, super::super::SourceSpan)> {
        self.original.source_location()
    }
}
// Values stay local and never implement Debug. The report contains only comparison flags.
struct PlainRoot {
    original: S<RelativeBindingName>,
    kind: RootKind,
    plain: S<RelativeBindingName>,
    counts: Counts,
}
fn same_bindings(a: &Bindings<RelativeBindingName>, b: &Bindings<RelativeBindingName>) -> bool {
    a.len() == b.len()
        && a.iter().all(|(name, attrs)| {
            b.get(name).is_some_and(|other| {
                attrs.len() == other.len()
                    && attrs.iter().all(|(key, value)| {
                        other.get(key).is_some_and(|other| {
                            crate::resource::same_plain_representation(value, other)
                        })
                    })
            })
        })
}
fn same_failure(a: &Result<(), RootInputFailure>, b: &Result<(), RootInputFailure>) -> bool {
    match (a, b) {
        (Ok(()), Ok(())) => true,
        (Err(a), Err(b)) => {
            fn error(failure: &RootInputFailure) -> &ParseError {
                match failure {
                    RootInputFailure::Backend(error) | RootInputFailure::Value(error) => error,
                }
            }
            a.is_backend() == b.is_backend() && observation::same_parse_error(error(a), error(b))
        }
        _ => false,
    }
}
pub(super) fn compare_invocation<E: ExportParamLike>(
    parsed: &mut File<E>,
    config: &ProviderContext,
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    resolution: ResolutionContext,
) -> Result<(), RootInputFailure> {
    use output_kind::EnrichedAuthored;
    let _invocation = observation::InvocationGuard::enter(resolution.requires_available());
    let mut saved = parsed.clone_for_resolver_observer(ExportView::new);
    let mut roots = VecDeque::new();
    let mut saved_bindings = None;
    let plain = {
        let _oracle = observation::OracleGuard::enter();
        resolve_resource_refs_output::<ParserPlaceholder, _>(
            &mut saved,
            config,
            session,
            resolution,
            &mut |original, kind, bindings, value| {
                if saved_bindings.is_none() {
                    saved_bindings = Some(bindings.clone());
                }
                roots.push_back(PlainRoot {
                    original: original.clone(),
                    kind,
                    plain: value.clone(),
                    counts: observation::take_oracle_root_counts(),
                });
                value
            },
        )
    };
    drop(saved);
    let mut same_inputs = true;
    let enriched = resolve_resource_refs_output::<EnrichedAuthored, E>(
        parsed,
        config,
        session,
        resolution,
        &mut |original, kind, bindings, lowered| {
            let projected = output_kind::project_for_comparison(&lowered);
            let matches = if let Some(expected) = roots.pop_front() {
                same_inputs &= std::mem::discriminant(&kind)
                    == std::mem::discriminant(&expected.kind)
                    && crate::resource::same_plain_representation(original, &expected.original)
                    && saved_bindings
                        .as_ref()
                        .is_some_and(|expected| same_bindings(expected, bindings));
                observation::set_oracle_root_counts(expected.counts);
                crate::resource::same_plain_representation(&projected, &expected.plain)
            } else {
                same_inputs = false;
                false
            };
            let paths = output_kind::observe_paths(&lowered);
            observation::publish(
                matches,
                matches!(kind, RootKind::Full),
                paths,
                output_kind::input_observation(original),
            );
            projected
        },
    );
    observation::finish_invocation(
        same_inputs,
        roots.is_empty(),
        same_failure(&plain, &enriched),
    );
    enriched
}

// Identity publication only. This is the ordinary plain instantiation of the
// same driver, with no oracle and no enriched nodes.
pub(super) fn collect_plain_invocation<E: ExportParamLike>(
    parsed: &mut File<E>,
    config: &ProviderContext,
    resolution: ResolutionContext,
) -> Result<(), RootInputFailure> {
    let _invocation = observation::InvocationGuard::enter(resolution.requires_available());
    let result = resolve_resource_refs_output::<ParserPlaceholder, E>(
        parsed,
        config,
        None,
        resolution,
        &mut |original, kind, _, value| {
            observation::publish(
                true,
                matches!(kind, RootKind::Full),
                Vec::new(),
                output_kind::input_observation(original),
            );
            value
        },
    );
    observation::finish_invocation(true, true, true);
    result
}
