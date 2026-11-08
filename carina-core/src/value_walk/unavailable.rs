//! Unavailable inspection uses kernel-issued protection and expression edges.
//! All policies re-enter Secret; saved verification selects which failures
//! count, while resolution excludes only a not-yet-selected coalesce default.

use super::*;
use crate::{config_loader::ResolutionContext, parser::ParseError, resource::InsideSecret};

pub(crate) fn find_required_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    context: ResolutionContext,
) -> Option<ParseError> {
    if !context.requires_available() {
        return None;
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
) -> bool {
    find_unavailable(value, policy).is_some()
}

fn find_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    policy: UnavailableSearchPolicy,
) -> Option<ParseError> {
    let mut search = UnavailableSearch {
        policy,
        errors: Vec::new(),
    };
    let evidence = match inspect(WalkRoot::syntactic(), value, &mut search) {
        Ok(located) => located.into_output(),
        Err(error) => return Some(ParseError::UserFunctionError(error.to_string())),
    };
    let site = evidence.obligations().first()?;
    let error = search.errors.into_iter().nth(*site.output())?;
    Some(if site.is_secret() {
        error.inside_secret()
    } else {
        error
    })
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
