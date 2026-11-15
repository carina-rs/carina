use super::*;

type UseIdentity = (NodeId, UseId, ExpansionInstanceId, ProjectionGeneration);

/// Findings that follow ordinary member selection without participating in it.
/// Keeping both streams in this bundle makes every existing container/union
/// transfer carry them together. The empty case allocates nothing.
#[derive(Clone, Default)]
pub(super) struct SupplementalFindings<C = crate::ConversionChannel>(
    Option<Box<SupplementalData<C>>>,
);

#[derive(Clone, Default)]
pub(super) struct SupplementalData<C = crate::ConversionChannel> {
    pub(super) authored: AssignmentFindings<C>,
    pub(super) references: RecoveredReferences,
}

impl<C: FindingsChannel> From<AssignmentFindings<C>> for SupplementalFindings<C> {
    fn from(authored: AssignmentFindings<C>) -> Self {
        if authored.local.is_empty()
            && authored.schema.is_empty()
            && authored.errors.is_empty()
            && authored.conversions.findings().is_empty()
            && authored.claim_effect == ClaimEffect::None
            && !authored.matched_destination
            && {
                #[cfg(test)]
                {
                    authored.deferred.is_empty()
                }
                #[cfg(not(test))]
                {
                    true
                }
            }
        {
            return Self::default();
        }
        Self(Some(Box::new(SupplementalData {
            authored,
            references: Default::default(),
        })))
    }
}

impl<C: FindingsChannel> SupplementalFindings<C> {
    fn data(&mut self) -> &mut SupplementalData<C> {
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

    pub(super) fn into_parts(self) -> SupplementalData<C> {
        self.0.map(|data| *data).unwrap_or_default()
    }

    pub(super) fn claim_effect(&self) -> ClaimEffect {
        self.0
            .as_ref()
            .map_or(ClaimEffect::None, |data| data.authored.claim_effect)
    }
    pub(super) fn matched_destination(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|data| data.authored.matched_destination)
    }
    pub(super) fn policy_errors(&self) -> &[(LeafPath, TypeError)] {
        self.0.as_ref().map_or(&[], |data| &data.authored.errors)
    }
    pub(super) fn conversion_errors(&self) -> &[crate::ConversionError] {
        self.0
            .as_ref()
            .map_or(&[], |data| data.authored.conversions.findings())
    }

    fn recover(&mut self, identity: UseIdentity, path: &FieldPath, error: TypeError) {
        self.data().references.0.push(RecoveredReference {
            identity,
            path: path.canonical(),
            error,
        });
    }
}

impl<C: FindingsChannel> SupplementalFindings<C> {
    pub(super) fn finish<P: ValueKind, B: BindingAddress, M: WalkMode<Channel = C>>(
        &mut self,
        callbacks: &WalkCallbacks<'_, P, B, M>,
    ) {
        if let Some(data) = &mut self.0 {
            callbacks.finish(&mut data.authored);
        }
    }
}

impl<C: FindingsChannel> InsideSecret for SupplementalFindings<C> {
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
pub(super) enum OrdinaryCheck<'a, P: ValueKind, B: BindingAddress, C = crate::ConversionChannel> {
    Direct(ProvenAssignmentCheck<'a, P, B, C>),
    #[cfg(test)]
    Observer(AssignmentCheck<'a, P, B, C>),
    References(&'a ReferencePolicy<'a, P, B, C>),
}
impl<P: ValueKind, B: BindingAddress, C> Copy for OrdinaryCheck<'_, P, B, C> {}
impl<P: ValueKind, B: BindingAddress, C> Clone for OrdinaryCheck<'_, P, B, C> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, P: ValueKind, B: BindingAddress, C> OrdinaryCheck<'a, P, B, C> {
    pub(super) fn call(
        self,
        attr: &CompiledAttributeType,
        value: CheckValue<'_, P, B>,
        path: &LeafPath,
        cursor: AuthoredCursor<'_, B>,
        proof: crate::parser::retained::NoConversion,
    ) -> Result<AssignmentFindings<C>, TypeError> {
        match self {
            Self::Direct(check) => check(attr, value, path, cursor, proof),
            #[cfg(test)]
            Self::Observer(check) => check(attr, value, path, cursor),
            Self::References(policy) => policy.ordinary.call(attr, value, path, cursor, proof),
        }
    }
}

#[cfg(test)]
impl<'a, P: ValueKind, B: BindingAddress, C, F> From<&'a F> for OrdinaryCheck<'a, P, B, C>
where
    F: Fn(
        &CompiledAttributeType,
        CheckValue<'_, P, B>,
        &LeafPath,
        AuthoredCursor<'_, B>,
    ) -> Result<AssignmentFindings<C>, TypeError>,
{
    fn from(callback: &'a F) -> Self {
        Self::Observer(callback)
    }
}

pub(super) struct ReferencePolicy<'a, P: ValueKind, B: BindingAddress, C = crate::ConversionChannel>
{
    ordinary: OrdinaryCheck<'a, P, B, C>,
    schema_source: &'a dyn Fn(&B) -> bool,
    reference: ReferenceJudge<'a, B>,
    scopes: std::cell::RefCell<Vec<UseIdentity>>,
}
impl<'a, P: ValueKind, B: BindingAddress, C> ReferencePolicy<'a, P, B, C> {
    pub(super) fn new(
        ordinary: impl Into<OrdinaryCheck<'a, P, B, C>>,
        reference: ReferenceCheck<'a, B>,
    ) -> Self {
        Self {
            ordinary: ordinary.into(),
            schema_source: reference.schema_source,
            reference: reference.check,
            scopes: Default::default(),
        }
    }
    fn enter(&self, identity: UseIdentity) -> ScopeGuard<'_, 'a, P, B, C> {
        // Qualification excludes an identity already being judged. Establish
        // the declaration's scope before any ordinary union-member trial.
        self.scopes.borrow_mut().push(identity);
        ScopeGuard { policy: self }
    }
}
struct ScopeGuard<'s, 'a, P: ValueKind, B: BindingAddress, C> {
    policy: &'s ReferencePolicy<'a, P, B, C>,
}
impl<P: ValueKind, B: BindingAddress, C> Drop for ScopeGuard<'_, '_, P, B, C> {
    fn drop(&mut self) {
        self.policy.scopes.borrow_mut().pop();
    }
}

#[derive(Default)]
pub(super) struct ReferenceMemo<'a, M: WalkMode = NoAuthored>(
    std::cell::RefCell<HashMap<CheckedNode<'a>, std::rc::Rc<WalkCheckedValue<'a, M>>>>,
);

impl<'a, M: WalkMode> ValueValidator<'a, '_, M> {
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
        assignment: Option<&WalkHandle<'w, P, B, M>>,
        extra: &mut AssignmentProbe<M::Channel>,
        wrappers: &mut Vec<AuthoredCursor<'w, B>>,
    ) -> Option<std::rc::Rc<WalkCheckedValue<'a, M>>>
    where
        'a: 'w,
    {
        let ClassifiedRef::Known(known) = value else {
            return None;
        };
        let walk = assignment?;
        // A wrapper's claim reaches its leaf through prepare; child steps reset it.
        if walk.claim.is_claimed() {
            return None;
        }
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
        let _scope = policy.enter(identity);
        let key = CheckedNode::new(attr, known.node_id(), ancestry, visited, Some(walk));
        if let Some(checked) = self.reference_checked.0.borrow().get(&key).cloned() {
            return Some(checked);
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
        if let Err(error) = reference {
            use crate::resource::SecretDiagnosticError;
            use crate::validation::admission::AdmissionReferenceError;
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
            if !checked
                .findings
                .located
                .iter()
                .any(|(ordinary_path, ordinary)| ordinary_path == path && same(ordinary))
                && !checked
                    .findings
                    .provider
                    .iter()
                    .any(|(ordinary_path, ordinary)| ordinary_path == path && same(ordinary))
            {
                checked.findings.authored.recover(identity, path, error);
            }
        }
        let checked = std::rc::Rc::new(checked);
        self.reference_checked
            .0
            .borrow_mut()
            .insert(key, checked.clone());
        Some(checked)
    }
}

#[cfg(test)]
mod tests;
