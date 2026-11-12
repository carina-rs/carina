use super::*;

type UseIdentity = (NodeId, UseId, ExpansionInstanceId, ProjectionGeneration);

#[derive(Debug, Clone, Copy, PartialEq)]
enum ErrorOrigin {
    Ordinary,
    RecoveredReference,
}

// Boxing keeps the origin attached to every error without enlarging a
// recursive CheckedValue frame. Transformations use the attached mark;
// projecting the diagnostic never determines its origin.
#[derive(Debug, Clone)]
pub(super) struct CheckedError(Box<ErrorDetail>);

#[derive(Debug, Clone)]
struct ErrorDetail {
    error: TypeError,
    origin: ErrorOrigin,
}

impl CheckedError {
    fn new(error: TypeError, origin: ErrorOrigin) -> Self {
        Self(Box::new(ErrorDetail { error, origin }))
    }

    pub(super) fn diagnostic(&self) -> &TypeError {
        &self.0.error
    }

    pub(super) fn into_diagnostic(self) -> TypeError {
        self.0.error
    }

    pub(super) fn wrap(self, wrap: impl FnOnce(TypeError) -> TypeError) -> Self {
        match self.0.origin {
            ErrorOrigin::Ordinary => Self::from(wrap(self.into_diagnostic())),
            ErrorOrigin::RecoveredReference => self,
        }
    }
}

impl From<TypeError> for CheckedError {
    fn from(error: TypeError) -> Self {
        Self::new(error, ErrorOrigin::Ordinary)
    }
}

impl InsideSecret for CheckedError {
    fn inside_secret(self) -> Self {
        let ErrorDetail { error, origin } = *self.0;
        let error = match origin {
            ErrorOrigin::Ordinary => error.inside_secret(),
            ErrorOrigin::RecoveredReference => protect_reference_error(error),
        };
        Self::new(error, origin)
    }
}

impl UnionFailureError for CheckedError {
    fn type_error(&self) -> &TypeError {
        self.diagnostic()
    }

    fn from_type_error(error: TypeError) -> Self {
        Self::from(error)
    }

    fn from_errors(first: Self, rest: impl IntoIterator<Item = Self>) -> Self {
        let ErrorDetail { error, origin } = *first.0;
        match UnionArmFailures::new(error, rest.into_iter().map(Self::into_diagnostic)) {
            Ok(failures) => Self::from(TypeError::UnionAlternativesMismatch { failures }),
            // A collapsed diagnostic keeps its first candidate's origin;
            // a newly generated multi-arm summary is an ordinary diagnostic.
            Err(error) => Self::new(error, origin),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct AssignmentError {
    diagnostic: (LeafPath, TypeError),
    origin: ErrorOrigin,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AssignmentErrors(Vec<AssignmentError>);

impl AssignmentErrors {
    pub(crate) fn ordinary(errors: impl IntoIterator<Item = (LeafPath, TypeError)>) -> Self {
        Self(
            errors
                .into_iter()
                .map(|diagnostic| AssignmentError {
                    diagnostic,
                    origin: ErrorOrigin::Ordinary,
                })
                .collect(),
        )
    }
    pub(crate) fn push(&mut self, diagnostic: (LeafPath, TypeError)) {
        self.0.push(AssignmentError {
            diagnostic,
            origin: ErrorOrigin::Ordinary,
        });
    }
    pub(super) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = &(LeafPath, TypeError)> {
        self.0.iter().map(|entry| &entry.diagnostic)
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
    pub(super) fn primary(&self) -> CheckedError {
        let entry = &self.0[0];
        CheckedError::new(entry.diagnostic.1.clone(), entry.origin)
    }
    pub(super) fn checked(&self) -> impl Iterator<Item = (&LeafPath, CheckedError)> {
        self.0.iter().map(|entry| {
            (
                &entry.diagnostic.0,
                CheckedError::new(entry.diagnostic.1.clone(), entry.origin),
            )
        })
    }
    pub(super) fn into_diagnostics(self) -> impl Iterator<Item = (LeafPath, TypeError)> {
        self.0.into_iter().map(|entry| entry.diagnostic)
    }
    pub(super) fn symbolic(&self) -> impl Iterator<Item = SymbolicFinding> {
        self.0.iter().map(|entry| {
            let (path, error) = &entry.diagnostic;
            match entry.origin {
                ErrorOrigin::Ordinary => SymbolicFinding::ordinary(path.clone(), error.clone()),
                ErrorOrigin::RecoveredReference => {
                    SymbolicFinding::recovered_reference(path.clone(), error.clone())
                }
            }
        })
    }
}
impl InsideSecret for AssignmentErrors {
    fn inside_secret(self) -> Self {
        Self(
            self.0
                .into_iter()
                .map(|entry| AssignmentError {
                    diagnostic: (
                        entry.diagnostic.0.inside_secret(),
                        match entry.origin {
                            ErrorOrigin::Ordinary => entry.diagnostic.1.inside_secret(),
                            ErrorOrigin::RecoveredReference => {
                                protect_reference_error(entry.diagnostic.1)
                            }
                        },
                    ),
                    origin: entry.origin,
                })
                .collect(),
        )
    }
}

fn protect_reference_error(error: TypeError) -> TypeError {
    use crate::resource::SecretDiagnosticError;
    use crate::validation::admission::AdmissionReferenceError;
    match error {
        TypeError::Reference { error, attribute } => match *error {
            SecretDiagnosticError::Public(AdmissionReferenceError::Computed(error)) => {
                TypeError::Reference {
                    error: Box::new(SecretDiagnosticError::Public(
                        AdmissionReferenceError::Computed(error.inside_secret()),
                    )),
                    attribute,
                }
            }
            error => TypeError::Reference {
                error: Box::new(error),
                attribute,
            }
            .inside_secret(),
        },
        error => error.inside_secret(),
    }
}

// A thin policy reference fits in the function pointer's unused vtable word.
// Ordinary callbacks and their transport retain main's layout and ownership.
pub(super) enum OrdinaryCheck<'a, P: ValueKind, B: BindingAddress> {
    Direct(AssignmentCheck<'a, P, B>),
    References(&'a ReferencePolicy<'a, P, B>),
}
impl<P: ValueKind, B: BindingAddress> Copy for OrdinaryCheck<'_, P, B> {}
impl<P: ValueKind, B: BindingAddress> Clone for OrdinaryCheck<'_, P, B> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, P: ValueKind, B: BindingAddress> OrdinaryCheck<'a, P, B> {
    pub(super) fn callback(self) -> AssignmentCheck<'a, P, B> {
        match self {
            Self::Direct(check) => check,
            Self::References(policy) => policy.ordinary,
        }
    }
}

pub(super) struct ReferencePolicy<'a, P: ValueKind, B: BindingAddress> {
    ordinary: AssignmentCheck<'a, P, B>,
    schema_source: &'a dyn Fn(&B) -> bool,
    reference: ReferenceJudge<'a, B>,
    scopes: std::cell::RefCell<Vec<UseIdentity>>,
}
impl<'a, P: ValueKind, B: BindingAddress> ReferencePolicy<'a, P, B> {
    pub(super) fn new(
        ordinary: AssignmentCheck<'a, P, B>,
        reference: ReferenceCheck<'a, B>,
    ) -> Self {
        Self {
            ordinary,
            schema_source: reference.schema_source,
            reference: reference.check,
            scopes: Default::default(),
        }
    }
    fn enter(&self, identity: UseIdentity) -> ScopeGuard<'_, 'a, P, B> {
        // Qualification excludes an identity already being judged. Establish
        // the declaration's scope before any ordinary union-member trial.
        self.scopes.borrow_mut().push(identity);
        ScopeGuard { policy: self }
    }
}
struct ScopeGuard<'s, 'a, P: ValueKind, B: BindingAddress> {
    policy: &'s ReferencePolicy<'a, P, B>,
}
impl<P: ValueKind, B: BindingAddress> Drop for ScopeGuard<'_, '_, P, B> {
    fn drop(&mut self) {
        self.policy.scopes.borrow_mut().pop();
    }
}

pub(super) struct ReferenceInput<'w, 's, P: PlaceholderKind, B: BindingAddress> {
    walk: &'w AssignmentWalk<'s, P, B>,
    policy: &'s ReferencePolicy<'s, P, B>,
    source: crate::parser::retained::SourceReference<'w, B>,
}
impl<'w, 's, P: PlaceholderKind, B: BindingAddress> ReferenceInput<'w, 's, P, B> {
    #[inline(never)]
    pub(super) fn for_walk(
        known: bool,
        assignment: Option<&'w AssignmentWalk<'s, P, B>>,
    ) -> Option<Self> {
        if !known {
            return None;
        }
        let walk = assignment?;
        let OrdinaryCheck::References(policy) = walk.callbacks.ordinary else {
            return None;
        };
        if walk.cursor.is_selection() {
            return None;
        }
        let source = walk.cursor.source_reference()?;
        let identity = source.identity;
        // Member trials inherit the enclosing declaration's judgement. They
        // keep the ordinary memo and never allocate reference state of their own.
        if policy.scopes.borrow().contains(&identity) || !(policy.schema_source)(source.binding) {
            return None;
        }
        Some(Self {
            walk,
            policy,
            source,
        })
    }
}

#[derive(Default)]
pub(super) struct ReferenceMemo<'a>(
    std::cell::RefCell<HashMap<CheckedNode<'a>, std::rc::Rc<CheckedValue<'a>>>>,
);

pub(super) struct AppliedReferenceValue<'a>(std::rc::Rc<CheckedValue<'a>>);
impl<'a> AppliedReferenceValue<'a> {
    #[inline(never)]
    pub(super) fn into_checked(self) -> CheckedValue<'a> {
        std::rc::Rc::unwrap_or_clone(self.0)
    }
}

/// Only this private state can carry an unapplied reference result. It cannot
/// enter assignment merges; finishing applies it before returning CheckedValue.
pub(super) struct PendingReferencedValue<'a>(Box<ReferenceResult<'a>>);
struct ReferenceResult<'a> {
    value: CheckedValue<'a>,
    reference: Result<AssignmentFindings, TypeError>,
    key: Option<CheckedNode<'a>>,
}
impl<'a> PendingReferencedValue<'a> {
    #[inline(never)]
    pub(super) fn finish(
        self,
        path: &FieldPath,
        memo: &ReferenceMemo<'a>,
    ) -> AppliedReferenceValue<'a> {
        let ReferenceResult {
            mut value,
            reference,
            key,
        } = *self.0;
        match reference {
            Ok(findings) => value.extend_assignment(path, findings),
            Err(error) => {
                use crate::resource::SecretDiagnosticError;
                use crate::validation::admission::AdmissionReferenceError;
                value.retain_primary_finding();
                // Equality only suppresses a duplicate diagnostic here; every
                // retained error carries its own origin through transformations.
                let same = |ordinary: &TypeError| {
                    ordinary == &error
                        || matches!((ordinary, &error),
                            (TypeError::TypeMismatch { types: ordinary }, TypeError::Reference { error, .. })
                            if matches!(error.as_ref(), SecretDiagnosticError::Public(AdmissionReferenceError::Computed(
                                crate::validation::DeferredAccessTypeError::TypeMismatch { types, .. }
                            )) if types == ordinary)
                        )
                };
                if !value
                    .findings
                    .located
                    .iter()
                    .any(|(ordinary_path, ordinary)| {
                        ordinary_path == path && same(ordinary.diagnostic())
                    })
                    && !value
                        .findings
                        .provider
                        .iter()
                        .any(|(ordinary_path, ordinary)| ordinary_path == path && same(ordinary))
                {
                    let checked_error =
                        CheckedError::new(error.clone(), ErrorOrigin::RecoveredReference);
                    value.reject(path, checked_error.clone());
                    value.findings.located.push((path.clone(), checked_error));
                    value.findings.assignment.errors.0.push(AssignmentError {
                        diagnostic: (path.canonical(), error),
                        origin: ErrorOrigin::RecoveredReference,
                    });
                }
            }
        }
        let value = std::rc::Rc::new(value);
        if let Some(key) = key {
            memo.0.borrow_mut().insert(key, value.clone());
        }
        AppliedReferenceValue(value)
    }
}

impl<'a> ValueValidator<'a, '_> {
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    pub(super) fn walk_reference<
        'w,
        'v: 'w,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: Option<&AssignmentWalk<'w, P, B>>,
        extra: &mut AssignmentProbe,
        wrappers: &mut Vec<AuthoredCursor<'w, B>>,
    ) -> Option<AppliedReferenceValue<'a>>
    where
        'a: 'w,
    {
        let input = ReferenceInput::for_walk(matches!(value, ClassifiedRef::Known(_)), assignment)?;
        let ReferenceInput {
            walk,
            policy,
            source,
        } = input;
        let _scope = policy.enter(source.identity);
        let key = match value {
            ClassifiedRef::Known(value) => Some(CheckedNode::new(
                attr,
                value.node_id(),
                ancestry,
                visited,
                Some(walk),
            )),
            _ => None,
        };
        if let Some(checked) = key
            .as_ref()
            .and_then(|key| self.reference_checked.0.borrow().get(key).cloned())
        {
            return Some(AppliedReferenceValue(checked));
        }
        let reference = {
            let mut resolving = visited.clone();
            resolve_refs_on_path(attr, self.defs, &mut resolving)
                .map_err(|error| error.into_validation_error())
                .and_then(|resolved| (policy.reference)(resolved.attr, &path.canonical(), source))
        };
        let mut checked =
            self.walk_value(attr, value, path, ancestry, visited, Some((*walk).clone()));
        self.complete_authored_walk(
            &mut checked,
            attr,
            value,
            path,
            Some((*walk).clone()),
            std::mem::take(extra),
            std::mem::take(wrappers),
        );
        checked.retain_primary_finding();
        Some(
            PendingReferencedValue(Box::new(ReferenceResult {
                value: checked,
                reference,
                key,
            }))
            .finish(path, &self.reference_checked),
        )
    }
}

#[cfg(test)]
mod tests;
