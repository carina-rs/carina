//! Unavailable inspection uses kernel-issued protection and expression edges.
//! All policies re-enter Secret; saved verification selects which failures
//! count, while resolution excludes only a not-yet-selected coalesce default.

use super::*;
use crate::{config_loader::ResolutionContext, parser::ParseError, resource::InsideSecret};

pub(crate) fn find_required_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    context: ResolutionContext,
) -> Result<Option<ParseError>, WalkError> {
    if !context.requires_available() {
        return Ok(None);
    }
    find_unavailable(value, UnavailableSearchPolicy::Required(context))
}

#[derive(Clone, Copy)]
pub(crate) enum UnavailableSearchPolicy {
    Required(ResolutionContext),
    /// Every candidate operand, including Secret and coalesce defaults.
    SavedCandidate,
    /// Only failures with a Secret ancestor, including coalesce defaults.
    SavedSecret,
}

impl UnavailableSearchPolicy {
    fn includes(self, at: &At<'_>) -> bool {
        // The former search inspected call arguments, not captured function
        // definitions/scopes or auxiliary access provenance. Keep that exact
        // expression surface while the kernel still inventories every edge.
        if at.expression_edges().iter().any(|edge| {
            matches!(
                edge,
                ExpressionEdge::FunctionDefinition | ExpressionEdge::FunctionScope(_)
            ) || matches!(edge, ExpressionEdge::ArgumentValue(index) if *index != 0)
        }) {
            return false;
        }
        match self {
            Self::Required(context) => context
                .at_expression_edges(at.expression_edges())
                .requires_available(),
            Self::SavedCandidate => true,
            Self::SavedSecret => at.is_secret(),
        }
    }
}

pub(crate) fn contains_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    policy: UnavailableSearchPolicy,
) -> Result<bool, WalkError> {
    find_unavailable(value, policy).map(|error| error.is_some())
}

fn find_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    policy: UnavailableSearchPolicy,
) -> Result<Option<ParseError>, WalkError> {
    let mut search = UnavailableSearch {
        policy,
        errors: Vec::new(),
    };
    let result =
        inspect(WalkRoot::syntactic(), value, &mut search).map(|located| located.into_output());
    finish_unavailable_search(result, search.errors)
}

fn finish_unavailable_search(
    result: Result<Evidence<usize, ()>, WalkError>,
    errors: Vec<ParseError>,
) -> Result<Option<ParseError>, WalkError> {
    let evidence = result?;
    let Some(site) = evidence.obligations().first() else {
        return Ok(None);
    };
    let error = errors
        .into_iter()
        .nth(*site.output())
        .ok_or(WalkError::ReceiptOrigin)?;
    Ok(Some(if site.is_secret() {
        error.inside_secret()
    } else {
        error
    }))
}

struct UnavailableSearch {
    policy: UnavailableSearchPolicy,
    errors: Vec<ParseError>,
}

impl Algebra<Inspect> for UnavailableSearch {
    type Output = Evidence<usize, ()>;

    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        scalar: ScopedScalar<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        scalar.finish(Evidence::empty())
    }

    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        items: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        items.inspect(self, Evidence::merge)
    }

    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        entries: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        entries.inspect(self, Evidence::merge)
    }

    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        expr: ExprChildren<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        let obligations = if self.policy.includes(at) {
            match &expr.children.parts {
                Parts::Unavailable {
                    name,
                    args: _,
                    reason,
                    source_line,
                } => {
                    let message = format!("{name}(): {reason}");
                    let error = match source_line.get() {
                        Some(line) => ParseError::InvalidExpression { line, message },
                        None => ParseError::UserFunctionError(message),
                    };
                    let handle = self.errors.len();
                    self.errors.push(error);
                    Some(handle)
                }
                Parts::List(_)
                | Parts::Map(_)
                | Parts::Coalesce {
                    value: _,
                    default: _,
                }
                | Parts::ModuleIf(_)
                | Parts::Arithmetic(_)
                | Parts::Interpolation(_)
                | Parts::Call {
                    name: _,
                    args: _,
                    source_line: _,
                }
                | Parts::ResolvedCall {
                    function: _,
                    functions: _,
                    args: _,
                    source_line: _,
                }
                | Parts::Access(_)
                | Parts::Evidence { value: _, edge: _ } => None,
            }
        } else {
            None
        };
        expr.with_evidence(obligations, []).inspect(self)
    }

    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        leaf: ScopedLeaf<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        leaf.finish(Evidence::empty())
    }

    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Inspect, P, B>,
    ) -> SecretStep<Inspect, Self::Output, P, B> {
        child.inspect(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_failure_sources_survive_parser_and_secret_error_boundaries() {
        let parser = ParseError::from(WalkError::Rejected).inside_secret();
        let secret =
            crate::value::SecretRehydrationError::from(WalkError::Rejected).inside_secret();
        for error in [
            &parser as &dyn std::error::Error,
            &secret as &dyn std::error::Error,
        ] {
            assert!(
                error
                    .source()
                    .is_some_and(|source| source.is::<WalkError>())
            );
        }
        assert!(matches!(parser, ParseError::ValueWalk(WalkError::Rejected)));
        assert!(matches!(
            secret,
            crate::value::SecretRehydrationError::ValueWalk(WalkError::Rejected)
        ));
    }

    #[test]
    fn unavailable_search_preserves_an_injected_walk_failure_type() {
        let failure = finish_unavailable_search(Err(WalkError::Rejected), Vec::new()).unwrap_err();
        let error: &dyn std::error::Error = &failure;
        assert_eq!(
            error.downcast_ref::<WalkError>(),
            Some(&WalkError::Rejected),
            "kernel failure lost its type: {failure:?}"
        );
    }
}
