//! Admission of evaluation errors at their grammar-owned protection boundary.
use super::{ParseError, PreprocessedFile, ProtectedParseFailure, Rule};
use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

/// Retained by deferred checks as well as immediate AST lowering. Positions are
/// projected only on failure, so successful lowering never recounts prefixes.
#[derive(Clone, Default)]
pub(crate) struct EvaluationSite {
    scope: Scope,
}

pub(crate) struct ScopedEvaluation {
    site: EvaluationSite,
    active: EvaluationScopes,
}
impl ScopedEvaluation {
    pub(crate) fn evaluate<T>(
        &self,
        evaluate: impl FnOnce() -> Result<T, ParseError>,
    ) -> Result<T, EvaluationError> {
        let site = self.active.capture(self.site.clone());
        let _entered = EnteredScope {
            active: self.active.0.clone(),
            previous: self.active.0.replace(site.scope.clone()),
        };
        site.evaluate(evaluate)
    }
}

/// Lexical child contexts share synchronous evaluation ownership. Retained
/// work snapshots the protection before the stack is unwound.
#[derive(Clone, Default)]
pub(super) struct EvaluationScopes(Rc<RefCell<Scope>>);

impl EvaluationScopes {
    pub(super) fn attach(&self, site: EvaluationSite) -> ScopedEvaluation {
        ScopedEvaluation {
            site,
            active: self.clone(),
        }
    }

    pub(super) fn capture(&self, site: EvaluationSite) -> EvaluationSite {
        let active = self.0.borrow();
        EvaluationSite {
            scope: match &*active {
                Scope::Public => site.scope,
                protected @ Scope::Protected { .. } => protected.clone(),
            },
        }
    }
}

struct EnteredScope {
    active: Rc<RefCell<Scope>>,
    previous: Scope,
}
impl Drop for EnteredScope {
    fn drop(&mut self) {
        self.active.replace(std::mem::take(&mut self.previous));
    }
}

#[derive(Clone, Default)]
enum Scope {
    #[default]
    Public,
    Protected {
        positions: Arc<crate::heredoc::PreprocessResult>,
        anchor: Range<usize>,
    },
}

impl PartialEq for EvaluationSite {
    fn eq(&self, other: &Self) -> bool {
        match (&self.scope, &other.scope) {
            (Scope::Public, Scope::Public) => true,
            (Scope::Protected { anchor: a, .. }, Scope::Protected { anchor: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl std::fmt::Debug for EvaluationSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match &self.scope {
            Scope::Public => "PublicEvaluation",
            Scope::Protected { .. } => "ProtectedEvaluation",
        })
    }
}

impl EvaluationSite {
    pub(super) fn from_pair(
        pair: &pest::iterators::Pair<'_, Rule>,
        file: &PreprocessedFile,
    ) -> Self {
        let start = pair.as_span().start();
        let range = start..start + pair.as_str().trim_end().len();
        Self::from_owner(
            file.protected_regions()
                .owner(range.clone())
                .filter(|owner| {
                    let owned = owner.owned();
                    owned.start <= range.start && range.end <= owned.end
                }),
            file,
        )
    }

    pub(super) fn from_location(
        location: &super::DeclarationLocation,
        file: &PreprocessedFile,
    ) -> Self {
        let span = location.span();
        Self::from_owner(
            file.protected_regions()
                .original_owner(span.start_byte()..span.end_byte(), file),
            file,
        )
    }

    fn from_owner(
        owner: Option<&super::protected_regions::SecretOwnedRange>,
        file: &PreprocessedFile,
    ) -> Self {
        match owner {
            Some(owner) => Self {
                scope: Scope::Protected {
                    positions: file.shared_position_map(),
                    anchor: owner.anchor(),
                },
            },
            None => Self::default(),
        }
    }

    pub(crate) fn evaluate<T>(
        &self,
        evaluate: impl FnOnce() -> Result<T, ParseError>,
    ) -> Result<T, EvaluationError> {
        match &self.scope {
            Scope::Public => evaluate().map_err(|error| EvaluationError(Failure::Public(error))),
            Scope::Protected { positions, anchor } => {
                // This branch cannot return or retain an arbitrary ParseError.
                // New parser variants are discarded without formatting them.
                protected_evaluation(evaluate, || {
                    let start = positions.original_position(anchor.start);
                    let end = positions.original_position(anchor.end);
                    ProtectedParseFailure::expression(
                        (start.line(), start.column()),
                        (end.line(), end.column()),
                    )
                })
                .map_err(|error| EvaluationError(Failure::Protected(error)))
            }
        }
    }
}

fn protected_evaluation<T>(
    evaluate: impl FnOnce() -> Result<T, ParseError>,
    failure: impl FnOnce() -> ProtectedParseFailure,
) -> Result<T, ProtectedParseFailure> {
    evaluate().map_err(|_| failure())
}

/// Only this module can construct an admitted result. Its protected arm has
/// neither source text nor an error/source chain capable of carrying it.
pub(crate) struct EvaluationError(Failure);
enum Failure {
    Public(ParseError),
    Protected(ProtectedParseFailure),
}
impl From<EvaluationError> for ParseError {
    fn from(error: EvaluationError) -> Self {
        match error.0 {
            Failure::Public(error) => error,
            Failure::Protected(error) => Self::ProtectedFile(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round13_deferred_checks_capture_the_protected_call_scope() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<EvaluationSite>();
        let input = "fn public(value) { value }\nlet hidden = secret(public('CANARY'))";
        let file = Arc::new(super::super::preprocess_file(input).unwrap());
        let pairs = file.parse().unwrap();
        let public = pairs
            .clone()
            .flatten()
            .find(|pair| pair.as_rule() == Rule::identifier && pair.as_str() == "public")
            .unwrap();
        let secret = pairs
            .flatten()
            .find(|pair| {
                pair.as_rule() == Rule::function_call
                    && pair
                        .clone()
                        .into_inner()
                        .next()
                        .is_some_and(|name| name.as_str() == "secret")
            })
            .unwrap();
        let config = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let ctx = super::super::ParseContext::new(
            &config,
            std::path::Path::new("main.crn"),
            file.clone(),
        );
        let location = ctx.declaration_location(&public);
        ctx.evaluation_site(&secret)
            .evaluate(|| {
                ctx.record_pending_function_type_check(
                    super::super::ast::PendingFunctionTypeCheck {
                        evaluation: ctx.evaluation_site_at(&location),
                        location: location.clone(),
                        function_name: "public".into(),
                        arguments: vec![],
                        source_line: 2,
                        source_column: None,
                        source_file: None,
                    },
                );
                Ok(())
            })
            .map_err(ParseError::from)
            .unwrap();
        let captured = ctx.pending_function_type_checks.borrow()[0]
            .evaluation
            .clone();
        let error = captured
            .evaluate::<()>(|| Err(ParseError::UserFunctionError("CANARY".into())))
            .map_err(ParseError::from)
            .unwrap_err();
        assert!(error.protected_failure().is_some(), "{error:?}");
        assert!(!format!("{error:?}").contains("CANARY"));
        let public = ctx
            .evaluation_site_at(&location)
            .evaluate::<()>(|| Err(ParseError::UserFunctionError("public error".into())))
            .map_err(ParseError::from)
            .unwrap_err();
        assert_eq!(
            public.to_string(),
            "User-defined function error: public error"
        );
    }
}
