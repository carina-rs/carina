use super::*;

type UseIdentity = (NodeId, UseId, ExpansionInstanceId, ProjectionGeneration);

/// Findings that follow ordinary member selection without participating in it.
/// Keeping both streams in this bundle makes every existing container/union
/// transfer carry them together. The empty case allocates nothing.
#[derive(Clone, Default)]
pub(super) struct SupplementalFindings(Option<Box<SupplementalData>>);

#[derive(Clone, Default)]
pub(super) struct SupplementalData {
    pub(super) authored: AssignmentFindings,
    pub(super) references: RecoveredReferences,
}

impl From<AssignmentFindings> for SupplementalFindings {
    fn from(authored: AssignmentFindings) -> Self {
        if authored.local.is_empty()
            && authored.schema.is_empty()
            && authored.errors.is_empty()
            && authored.deferred.is_empty()
        {
            return Self::default();
        }
        Self(Some(Box::new(SupplementalData {
            authored,
            references: Default::default(),
        })))
    }
}

impl SupplementalFindings {
    fn data(&mut self) -> &mut SupplementalData {
        self.0.get_or_insert_with(Default::default)
    }

    pub(super) fn extend(&mut self, other: impl Into<Self>) {
        let Some(other) = other.into().0 else {
            return;
        };
        let data = self.data();
        data.authored.extend(other.authored);
        data.references.0.extend(other.references.0);
    }

    pub(super) fn into_parts(self) -> SupplementalData {
        self.0.map(|data| *data).unwrap_or_default()
    }

    pub(super) fn finish<P: ValueKind, B: BindingAddress>(
        &mut self,
        callbacks: &WalkCallbacks<'_, P, B>,
    ) {
        if let Some(data) = &mut self.0 {
            callbacks.finish(&mut data.authored);
        }
    }

    fn recover(&mut self, identity: UseIdentity, path: &FieldPath, error: TypeError) {
        self.data().references.0.push(RecoveredReference {
            identity,
            path: path.canonical(),
            error,
        });
    }
}

impl InsideSecret for SupplementalFindings {
    fn inside_secret(self) -> Self {
        Self(self.0.map(|data| {
            Box::new(SupplementalData {
                authored: data.authored.inside_secret(),
                references: RecoveredReferences(
                    data.references
                        .0
                        .into_iter()
                        .map(|finding| RecoveredReference {
                            identity: finding.identity,
                            path: finding.path.inside_secret(),
                            error: protect_reference_error(finding.error),
                        })
                        .collect(),
                ),
            })
        }))
    }
}

#[derive(Clone)]
struct RecoveredReference {
    identity: UseIdentity,
    path: LeafPath,
    error: TypeError,
}

#[derive(Clone, Default)]
pub(super) struct RecoveredReferences(Vec<RecoveredReference>);

impl RecoveredReferences {
    pub(super) fn attach(self, findings: &mut ValueValidationFindings) {
        let mut seen = std::collections::HashSet::new();
        for RecoveredReference {
            identity,
            path,
            error,
        } in self.0
        {
            if !seen.insert(identity) {
                continue;
            }
            findings.additional_local_errors.push(error.clone());
            findings.located.push((path.clone(), error.clone()));
            findings
                .symbolic
                .push(SymbolicFinding::recovered_reference(path, error));
        }
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
    reference: Result<(), TypeError>,
    identity: UseIdentity,
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
            identity,
            key,
        } = *self.0;
        if let Err(error) = reference {
            use crate::resource::SecretDiagnosticError;
            use crate::validation::admission::AdmissionReferenceError;
            value.retain_primary_finding();
            // Intentionally leaf-only: suppress a direct ordinary duplicate.
            // Container summaries wrap child errors and keep their own paths;
            // they are never equated with a recovered source judgement.
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
                .any(|(ordinary_path, ordinary)| ordinary_path == path && same(ordinary))
                && !value
                    .findings
                    .provider
                    .iter()
                    .any(|(ordinary_path, ordinary)| ordinary_path == path && same(ordinary))
            {
                value.findings.authored.recover(identity, path, error);
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
        let identity = source.identity;
        let _scope = policy.enter(identity);
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
        let reference = {
            let mut resolving = visited.clone();
            resolve_refs_on_path(attr, self.defs, &mut resolving)
                .map_err(|error| error.into_validation_error())
                .and_then(|resolved| (policy.reference)(resolved.attr, &path.canonical(), source))
        };
        Some(
            PendingReferencedValue(Box::new(ReferenceResult {
                value: checked,
                reference,
                identity,
                key,
            }))
            .finish(path, &self.reference_checked),
        )
    }
}

#[cfg(test)]
mod tests;
