use super::*;

type UseIdentity = (NodeId, UseId, ExpansionInstanceId, ProjectionGeneration);

#[derive(Debug, Clone, Copy, PartialEq)]
enum ErrorOrigin {
    Ordinary,
    RecoveredReference,
}

#[derive(Debug, Clone, PartialEq)]
struct AssignmentError {
    diagnostic: (LeafPath, TypeError),
    origin: ErrorOrigin,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AssignmentErrors(Vec<AssignmentError>);

impl AssignmentErrors {
    #[inline(never)]
    pub(super) fn owns_recovered(&self, error: &TypeError) -> bool {
        self.0.iter().any(|entry| {
            entry.origin == ErrorOrigin::RecoveredReference && &entry.diagnostic.1 == error
        })
    }

    pub(super) fn protect_error(&self, error: TypeError) -> TypeError {
        if self.owns_recovered(&error) {
            protect_reference_error(error)
        } else {
            error.inside_secret()
        }
    }
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
    pub(super) fn primary(&self) -> &TypeError {
        &self.0[0].diagnostic.1
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
    reference: AssignmentCheck<'a, P, B>,
    scopes: std::cell::RefCell<Vec<ReferenceScope>>,
}
#[derive(Clone, PartialEq, Eq, Hash)]
struct ReferenceScope {
    identity: UseIdentity,
    declaration: *const CompiledAttributeType,
}
impl<'a, P: ValueKind, B: BindingAddress> ReferencePolicy<'a, P, B> {
    pub(super) fn new(
        ordinary: AssignmentCheck<'a, P, B>,
        reference: ReferenceCheck<'a, P, B>,
    ) -> Self {
        Self {
            ordinary,
            schema_source: reference.schema_source,
            reference: reference.check,
            scopes: Default::default(),
        }
    }
    fn enter(
        &self,
        identity: UseIdentity,
        attr: &CompiledAttributeType,
    ) -> ScopeGuard<'_, 'a, P, B> {
        let mut scopes = self.scopes.borrow_mut();
        // The declaration is fixed before trials run. Re-entering this use
        // inherits its enclosing sink, independently of callback execution order.
        let (scope, owner) =
            if let Some(scope) = scopes.iter().rev().find(|scope| scope.identity == identity) {
                (scope.clone(), false)
            } else {
                let scope = ReferenceScope {
                    identity,
                    declaration: attr,
                };
                scopes.push(scope.clone());
                (scope, true)
            };
        ScopeGuard {
            policy: self,
            scope,
            owner,
        }
    }
}
struct ScopeGuard<'s, 'a, P: ValueKind, B: BindingAddress> {
    policy: &'s ReferencePolicy<'a, P, B>,
    scope: ReferenceScope,
    owner: bool,
}
impl<P: ValueKind, B: BindingAddress> Drop for ScopeGuard<'_, '_, P, B> {
    fn drop(&mut self) {
        if self.owner {
            self.policy.scopes.borrow_mut().pop();
        }
    }
}

pub(super) struct ReferenceInput<'w, 's, P: PlaceholderKind, B: BindingAddress>(
    &'w AssignmentWalk<'s, P, B>,
);
impl<'w, 's, P: PlaceholderKind, B: BindingAddress> ReferenceInput<'w, 's, P, B> {
    #[inline(never)]
    pub(super) fn for_walk(
        known: bool,
        assignment: &'w Option<AssignmentWalk<'s, P, B>>,
    ) -> Option<Self> {
        if !known {
            return None;
        }
        let walk = assignment.as_ref()?;
        let OrdinaryCheck::References(policy) = walk.callbacks.ordinary else {
            return None;
        };
        let source = walk.cursor.source_reference()?;
        let identity = source.identity;
        // Member trials inherit the enclosing declaration's judgement. They
        // keep the ordinary memo and never allocate reference state of their own.
        if policy
            .scopes
            .borrow()
            .iter()
            .any(|scope| scope.identity == identity)
            || !(policy.schema_source)(source.binding)
        {
            return None;
        }
        Some(Self(walk))
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ReferenceNode<'a> {
    node: CheckedNode<'a>,
    scope: ReferenceScope,
}
#[derive(Default)]
pub(super) struct ReferenceMemo<'a>(
    std::cell::RefCell<HashMap<ReferenceNode<'a>, std::rc::Rc<CheckedValue<'a>>>>,
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
    reference: Option<Result<AssignmentFindings, TypeError>>,
    key: Option<ReferenceNode<'a>>,
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
            None => {}
            Some(Ok(findings)) => value.extend_assignment(path, findings),
            Some(Err(error)) => {
                use crate::resource::SecretDiagnosticError;
                use crate::validation::admission::AdmissionReferenceError;
                value.retain_primary_finding();
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
                    .chain(&value.findings.provider)
                    .any(|(ordinary_path, ordinary)| ordinary_path == path && same(ordinary))
                {
                    value.reject(path, error.clone());
                    value.findings.located.push((path.clone(), error.clone()));
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
    pub(super) fn walk_reference_prepared<
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
        input: ReferenceInput<'_, 'w, P, B>,
        extra: &mut AssignmentProbe,
        wrappers: &mut Vec<AuthoredCursor<'w, B>>,
    ) -> AppliedReferenceValue<'a>
    where
        'a: 'w,
    {
        let walk = input.0;
        let (OrdinaryCheck::References(policy), Some(source)) =
            (walk.callbacks.ordinary, walk.cursor.source_reference())
        else {
            // ReferenceInput borrows the immutable walk it qualified. Record
            // missing evidence if that private construction invariant changes.
            return AppliedReferenceValue(std::rc::Rc::new(CheckedValue::rejected(
                path,
                seam_error(crate::parser::retained::MissingEvidence::new(
                    "qualified source reference",
                )),
            )));
        };
        let identity = source.identity;
        let scope = policy.enter(identity, attr);
        let key = match value {
            ClassifiedRef::Known(value) => Some(ReferenceNode {
                node: CheckedNode::new(attr, value.node_id(), ancestry, visited, Some(walk)),
                scope: scope.scope.clone(),
            }),
            _ => None,
        };
        if let Some(checked) = key
            .as_ref()
            .and_then(|key| self.reference_checked.0.borrow().get(key).cloned())
        {
            return AppliedReferenceValue(checked);
        }
        let reference = if scope.owner {
            let mut resolving = visited.clone();
            Some(
                resolve_refs_on_path(attr, self.defs, &mut resolving)
                    .map_err(|error| error.into_validation_error())
                    .and_then(|resolved| {
                        (policy.reference)(
                            resolved.attr,
                            CheckValue::Current(value.diagnostic_value()),
                            &path.canonical(),
                            walk.cursor.clone(),
                        )
                    }),
            )
        } else {
            None
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
        PendingReferencedValue(Box::new(ReferenceResult {
            value: checked,
            reference,
            key,
        }))
        .finish(path, &self.reference_checked)
    }
}

#[cfg(test)]
mod tests;
