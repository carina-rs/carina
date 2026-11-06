//! Local checks and provider dispatch share a value traversal and Union choice.

use super::*;

/// Proof that this exact declaration accepted the value, including its canonical
/// scalar spelling. Only the value validator can construct this proof.
pub struct LocallyValidatedCustomType<'a> {
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: Value,
    canonical: crate::provider::CanonicalCustomTypeValue,
    requires_provider: bool,
}

impl LocallyValidatedCustomType<'_> {
    pub fn declaration(&self) -> crate::parser::IdentifiedCustomTypeDeclaration<'_> {
        self.declaration
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn requires_provider(&self) -> bool {
        self.requires_provider
    }

    pub fn canonical(&self) -> &crate::provider::CanonicalCustomTypeValue {
        &self.canonical
    }
}

/// A provider bridge can consume only a locally validated, canonical leaf.
pub trait ValidatedCustomTypeLookup: Send + Sync {
    fn validate(&self, value: LocallyValidatedCustomType<'_>) -> Result<(), TypeError>;

    /// Preserve owner attribution when local canonicalization cannot construct
    /// a dispatch proof. This reports a failure without invoking the provider.
    fn canonicalization_error(
        &self,
        _declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
        error: TypeError,
    ) -> TypeError {
        error
    }
}

// Schema tests inject routing failures without needing a provider factory.
#[cfg(test)]
impl<F> ValidatedCustomTypeLookup for F
where
    F: Fn(&CompiledAttributeType, &Value) -> Result<(), TypeError> + Send + Sync,
{
    fn validate(&self, value: LocallyValidatedCustomType<'_>) -> Result<(), TypeError> {
        self(value.declaration.declared_type(), value.value())
    }
}

/// Cached findings for one attribute. The primary error retains the CLI's
/// established nesting; paths retain the LSP's established element locations.
#[derive(Default)]
pub struct ValueValidationFindings {
    local_error: Option<TypeError>,
    local: Vec<(FieldPath, TypeError)>,
    pub(super) provider: Vec<(FieldPath, TypeError)>,
}

impl ValueValidationFindings {
    pub(super) fn into_local_result(self) -> Result<(), TypeError> {
        self.local_error.map_or(Ok(()), Err)
    }

    pub(super) fn into_local_findings(self) -> Vec<(FieldPath, TypeError)> {
        self.local
    }

    pub fn local_error(&self) -> Option<&TypeError> {
        self.local_error.as_ref()
    }

    pub fn local_findings(&self) -> &[(FieldPath, TypeError)] {
        &self.local
    }

    fn first_error(&self) -> Option<&TypeError> {
        self.local_error
            .as_ref()
            .or_else(|| self.provider.first().map(|(_, error)| error))
    }

    pub(super) fn into_errors(self) -> Vec<TypeError> {
        self.local_error
            .into_iter()
            .chain(self.provider.into_iter().map(|(_, error)| error))
            .collect()
    }
}

impl InsideSecret for ValueValidationFindings {
    fn inside_secret(self) -> Self {
        Self {
            local_error: self.local_error.map(InsideSecret::inside_secret),
            local: self.local.inside_secret(),
            provider: self.provider.inside_secret(),
        }
    }
}

/// One validation report, usable by both CLI and LSP without revalidation.
#[derive(Default)]
pub struct ResourceValueValidation {
    pub(super) errors: Vec<(bool, TypeError)>,
    pub(super) attributes: HashMap<String, ValueValidationFindings>,
}

impl ResourceValueValidation {
    pub fn attribute(&self, name: &str) -> Option<&ValueValidationFindings> {
        self.attributes.get(name)
    }

    /// The boolean identifies provider findings, which retain their own
    /// diagnostic even when another leaf in the attribute failed locally.
    pub fn errors(&self) -> impl Iterator<Item = (bool, &TypeError)> {
        self.errors
            .iter()
            .map(|(provider, error)| (*provider, error))
    }

    pub(super) fn into_errors(self) -> Vec<TypeError> {
        self.errors.into_iter().map(|(_, error)| error).collect()
    }

    pub(super) fn into_result(self) -> Result<(), Vec<TypeError>> {
        let errors = self.into_errors();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

struct CheckedValue<'a> {
    fit: Option<Fit<'a>>,
    findings: ValueValidationFindings,
}

impl<'a> CheckedValue<'a> {
    fn accepted(
        attr: &'a CompiledAttributeType,
        exactness: FitExactness,
        kind: FitKind<'a>,
    ) -> Self {
        Self {
            fit: Some(Fit::with_kind(exactness, attr, kind)),
            findings: Default::default(),
        }
    }

    fn rejected(path: &FieldPath, error: TypeError) -> Self {
        Self {
            fit: None,
            findings: ValueValidationFindings {
                local_error: Some(error.clone()),
                local: vec![(path.clone(), error)],
                provider: Vec::new(),
            },
        }
    }

    fn accepts(&self) -> bool {
        self.fit.is_some() && self.findings.provider.is_empty()
    }
}

struct ValueValidator<'a> {
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    lookup: Option<CustomTypeLookup<'a>>,
}

pub(super) fn validate_value_locally<P: PlaceholderKind, B: BindingAddress>(
    attr: &CompiledAttributeType,
    value: &Value<P, B>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
) -> ValueValidationFindings {
    ValueValidator { defs, lookup: None }
        .walk(attr, value, &FieldPath::new(), ancestry, &mut Vec::new())
        .findings
}

#[cfg(test)]
pub(super) fn select_union_member_on_path<'a>(
    members: &'a [CompiledAttributeType],
    value: &Value,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    visited: &mut Vec<&'a str>,
) -> Option<&'a CompiledAttributeType> {
    ValueValidator { defs, lookup: None }
        .union(
            members,
            value,
            &FieldPath::new(),
            FieldAncestry::WRITABLE,
            visited,
        )
        .ok()
        .map(|selected| selected.member)
}

pub(super) fn validate_value_with_lookup<P: PlaceholderKind, B: BindingAddress>(
    attr: &CompiledAttributeType,
    value: &Value<P, B>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    lookup: CustomTypeLookup<'_>,
) -> ValueValidationFindings {
    ValueValidator {
        defs,
        lookup: Some(lookup),
    }
    .walk(
        attr,
        value,
        &FieldPath::new(),
        FieldAncestry::WRITABLE,
        &mut Vec::new(),
    )
    .findings
}

/// Standalone declarations enter the same leaf validator as resource values.
/// The continuation receives the sealed proof and cannot be called on a failed
/// local check. This also gives annotations their local declaration guard.
pub(crate) fn validate_custom_type_leaf<'a, R>(
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: &Value,
    validated: impl FnOnce(Option<LocallyValidatedCustomType<'a>>) -> R,
) -> Result<R, TypeError> {
    let validator = ValueValidator {
        defs: empty_defs_for_schema_walks(),
        lookup: None,
    };
    let checked = validator.walk(
        declaration.declared_type(),
        value,
        &FieldPath::new(),
        FieldAncestry::WRITABLE,
        &mut Vec::new(),
    );
    if let Some(error) = checked.findings.first_error() {
        return Err(error.clone());
    }
    let proof = validator
        .canonical_leaf(declaration.declared_type(), value, checked.fit.as_ref())
        .map_err(|failure| *failure.error)?;
    Ok(validated(proof))
}

struct CanonicalLeafError<'a> {
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    error: Box<TypeError>,
}

impl<'a> ValueValidator<'a> {
    fn walk<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        value: &Value<P, B>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> CheckedValue<'a> {
        if let Value::Deferred(DeferredValue::Secret(inner)) = value {
            // The proof stays inside this boundary. Only masked findings leave.
            let checked = inner.try_project_plaintext(|value| {
                let checked = self.walk(attr, value, path, ancestry.inside_secret(), visited);
                if checked.findings.first_error().is_some() {
                    Err(Box::new(checked.findings))
                } else {
                    Ok(checked.fit)
                }
            });
            return match checked {
                Ok(Some(fit)) => {
                    CheckedValue::accepted(attr, fit.exactness, FitKind::Secret(Box::new(fit)))
                }
                Ok(None) => CheckedValue {
                    fit: None,
                    findings: Default::default(),
                },
                Err(findings) => CheckedValue {
                    fit: None,
                    findings: *findings,
                },
            };
        }
        if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
            let mut left = self.walk(attr, value, path, ancestry, &mut Vec::new());
            let right = self.walk(attr, default, path, ancestry, &mut Vec::new());
            if left.findings.local_error.is_none() {
                left.findings.local_error = right.findings.local_error;
            }
            // Deferred coalesces have one authored parent location. Keep the
            // established primary local diagnostic there, while retaining every
            // provider finding from the independently known branches.
            left.findings.local = left
                .findings
                .local_error
                .as_ref()
                .map(|error| vec![(path.clone(), error.clone())])
                .unwrap_or_default();
            left.findings.provider.extend(right.findings.provider);
            left.fit = left.fit.zip(right.fit).map(|(value, default)| {
                let mut exactness = value.exactness;
                exactness.combine(default.exactness);
                Fit::with_kind(
                    exactness,
                    attr,
                    FitKind::Coalesce {
                        value: Box::new(value),
                        default: Box::new(default),
                    },
                )
            });
            return left;
        }
        if let Value::Deferred(DeferredValue::ModuleIf(branches)) = value {
            let mut checked = CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Deferred);
            for branch in &branches.values()[1..] {
                let branch = self.walk(attr, branch, path, ancestry, &mut Vec::new());
                if checked.findings.local_error.is_none() {
                    checked.findings.local_error = branch.findings.local_error;
                }
                checked.findings.local.extend(branch.findings.local);
                checked.findings.provider.extend(branch.findings.provider);
            }
            if checked.findings.local_error.is_some() {
                checked.fit = None;
            }
            return checked;
        }
        let resolution = match resolve_refs_on_path(attr, self.defs, visited) {
            Ok(resolution) => resolution,
            Err(error) => return CheckedValue::rejected(path, error.into_validation_error()),
        };
        let checked = self.walk_resolved(resolution.attr, value, path, ancestry, visited);
        visited.truncate(resolution.restore_len);
        checked
    }

    fn walk_resolved<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        value: &Value<P, B>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> CheckedValue<'a> {
        if let AttrTypeKind::UnloadedSchemaType(_) = &attr.kind {
            return CheckedValue::rejected(
                path,
                crate::parser::CustomTypeValidationError::ProvidersNotLoaded.into(),
            );
        }
        if let AttrTypeKind::Union(members) = &attr.kind {
            return match self.union(members.as_raw_slice(), value, path, ancestry, visited) {
                Ok(selected) => CheckedValue::accepted(
                    attr,
                    selected.fit.exactness,
                    FitKind::Union {
                        member: selected.member,
                        fit: Box::new(selected.fit),
                    },
                ),
                Err(findings) => CheckedValue {
                    fit: None,
                    findings: *findings,
                },
            };
        }
        let Some(concrete) = value.as_concrete() else {
            // Arithmetic has a known numeric result shape even while deferred.
            let fit = fit_value_to_type_on_path(
                attr,
                value,
                self.defs,
                ancestry,
                CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
                visited,
            );
            return match fit {
                Some(fit) => CheckedValue {
                    fit: Some(fit),
                    findings: Default::default(),
                },
                None => CheckedValue::rejected(
                    path,
                    TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
                ),
            };
        };
        match (&attr.kind, concrete) {
            (
                AttrTypeKind::List {
                    element_type,
                    constraints,
                    ..
                },
                ConcreteValueRef::List(items),
            ) => self.list(
                attr,
                element_type,
                *constraints,
                items,
                false,
                path,
                ancestry,
            ),
            (
                AttrTypeKind::List {
                    element_type,
                    constraints,
                    ..
                },
                ConcreteValueRef::StringList(items),
            ) => {
                let items = items
                    .iter()
                    .map(|item| Value::Concrete(ConcreteValue::String(item.clone())))
                    .collect::<Vec<Value<P, B>>>();
                self.list(
                    attr,
                    element_type,
                    *constraints,
                    &items,
                    true,
                    path,
                    ancestry,
                )
            }
            (
                AttrTypeKind::Struct {
                    name,
                    fields,
                    field_groups,
                },
                ConcreteValueRef::Map(map),
            ) => {
                let mut checked =
                    CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Struct(Vec::new()));
                for field in fields {
                    if ancestry.effective_input_mode(field).is_required()
                        && !struct_field_is_present(fields, map, field, ancestry)
                    {
                        let error = TypeError::MissingRequired {
                            name: field.name.clone(),
                        };
                        checked
                            .findings
                            .local_error
                            .get_or_insert_with(|| error.clone());
                        checked
                            .findings
                            .local
                            .push((path.push_field(field.name.clone()), error));
                    }
                }
                let groups = violated_struct_field_groups(fields, field_groups, map, ancestry)
                    .collect::<Vec<_>>();
                for group in &groups {
                    checked.findings.local.push((
                        path.clone(),
                        TypeError::FieldGroupViolation {
                            path: path.clone(),
                            group: (*group).clone(),
                        },
                    ));
                }
                let known = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>();
                let mut fits = Vec::new();
                let mut exactness = FitExactness::EXACT;
                for (key, value) in map {
                    let child_path = path.push_field(key.clone());
                    let child = match struct_field_for_key(fields, key, ancestry) {
                        Some(field) => match validate_struct_field_assignment(field, key, ancestry)
                        {
                            Some(error) => CheckedValue::rejected(&child_path, error),
                            None => self.walk(
                                &field.field_type,
                                value,
                                &child_path,
                                ancestry.descend(field),
                                &mut Vec::new(),
                            ),
                        },
                        None => CheckedValue::rejected(
                            &child_path,
                            TypeError::UnknownStructField {
                                struct_name: name.clone(),
                                field: key.clone(),
                                suggestion: suggest_similar_name(key, &known),
                            },
                        ),
                    };
                    if let Some(error) = child.findings.local_error {
                        // Unknown/writability errors are already anchored to their field.
                        let error = if struct_field_for_key(fields, key, ancestry).is_none() {
                            error
                        } else {
                            error.at_struct_field(key)
                        };
                        checked.findings.local_error.get_or_insert(error);
                    }
                    checked.findings.local.extend(child.findings.local);
                    checked.findings.provider.extend(child.findings.provider);
                    if let Some(fit) = child.fit {
                        exactness.combine(fit.exactness);
                        fits.push(fit);
                    }
                }
                if let Some(group) = groups.first() {
                    checked.findings.local_error.get_or_insert_with(|| {
                        TypeError::FieldGroupViolation {
                            path: FieldPath::new(),
                            group: (*group).clone(),
                        }
                    });
                }
                checked.fit = checked
                    .findings
                    .local_error
                    .is_none()
                    .then(|| Fit::with_kind(exactness, attr, FitKind::Struct(fits)));
                checked
            }
            (
                AttrTypeKind::Map {
                    key,
                    value: value_type,
                    constraints,
                },
                ConcreteValueRef::Map(map),
            ) => {
                let mut checked = CheckedValue::accepted(
                    attr,
                    FitExactness::GENERIC_MAP,
                    FitKind::Map(Vec::new()),
                );
                if let Err(error) = constraints.check(map.len()) {
                    checked.findings.local_error = Some(error.clone());
                    checked.findings.local.push((path.clone(), error));
                }
                let mut fits = Vec::new();
                let mut exactness = FitExactness::GENERIC_MAP;
                let mut first_value_error = None;
                for (map_key, map_value) in map {
                    let next_path = path.push_field(map_key.clone());
                    let key_value: Value<P, B> = lift_map_key(key, map_key, self.defs);
                    let key_checked =
                        self.walk(key, &key_value, &next_path, ancestry, &mut Vec::new());
                    if let Some(error) = key_checked.findings.local_error {
                        let error = TypeError::MapKeyError {
                            key: map_key.clone(),
                            inner: Box::new(error),
                        };
                        checked
                            .findings
                            .local_error
                            .get_or_insert_with(|| error.clone());
                        checked.findings.local.push((next_path.clone(), error));
                    }
                    checked
                        .findings
                        .provider
                        .extend(key_checked.findings.provider);
                    let child =
                        self.walk(value_type, map_value, &next_path, ancestry, &mut Vec::new());
                    let has_coarse_error =
                        child.findings.local.iter().any(|(_, error)| {
                            !matches!(error, TypeError::FieldGroupViolation { .. })
                        });
                    if let Some(error) = child.findings.local_error {
                        let error = error.at_map_value(map_key);
                        checked
                            .findings
                            .local_error
                            .get_or_insert_with(|| error.clone());
                        if has_coarse_error {
                            first_value_error.get_or_insert(error);
                        }
                    }
                    checked
                        .findings
                        .local
                        .extend(child.findings.local.into_iter().filter(|(_, error)| {
                            matches!(error, TypeError::FieldGroupViolation { .. })
                        }));
                    checked.findings.provider.extend(child.findings.provider);
                    if let (Some(key_fit), Some(value_fit)) = (key_checked.fit, child.fit) {
                        exactness.combine(key_fit.exactness);
                        exactness.combine(value_fit.exactness);
                        fits.push(MapEntryFit {
                            canonical_key: canonical_enum_key_from_fit(&key_fit),
                            value: value_fit,
                        });
                    }
                }
                if let Some(error) = first_value_error {
                    checked.findings.local.push((path.clone(), error));
                }
                checked.fit = checked
                    .findings
                    .local_error
                    .is_none()
                    .then(|| Fit::with_kind(exactness, attr, FitKind::Map(fits)));
                checked
            }
            (AttrTypeKind::Struct { name, .. }, ConcreteValueRef::List(_)) => {
                CheckedValue::rejected(
                    path,
                    TypeError::BlockSyntaxNotAllowed {
                        attribute: name.clone(),
                    },
                )
            }
            (
                AttrTypeKind::List { .. } | AttrTypeKind::Map { .. } | AttrTypeKind::Struct { .. },
                _,
            ) => CheckedValue::rejected(
                path,
                TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
            ),
            _ => self.leaf(attr, value, path, ancestry, visited),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn list<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        inner: &'a CompiledAttributeType,
        constraints: ListConstraints,
        items: &[Value<P, B>],
        string_list: bool,
        path: &FieldPath,
        ancestry: FieldAncestry,
    ) -> CheckedValue<'a> {
        let mut children = items
            .iter()
            .enumerate()
            .map(|(i, value)| {
                self.walk(inner, value, &path.push_index(i), ancestry, &mut Vec::new())
            })
            .collect::<Vec<_>>();
        let mut child_fits = children.iter_mut().map(|child| child.fit.take());
        let mut checked = CheckedValue {
            fit: None,
            findings: Default::default(),
        };
        let mut fits = Vec::new();
        let mut exactness = FitExactness::EXACT;
        let mut constraints_errors = Vec::new();
        for result in constraints.check(
            items.iter(),
            self.defs,
            ancestry,
            CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
            |_| child_fits.next().flatten(),
        ) {
            match result {
                Ok((index, fit)) => {
                    if let Some(fit) = fit {
                        exactness.combine(fit.exactness);
                        fits.push(fit);
                    }
                    constraints_errors.push((Some(index), None));
                }
                Err(ListConstraintError { index, error }) => {
                    constraints_errors.push((index, Some(*error)))
                }
            }
        }
        // CLI keeps count/first-element precedence; the LSP keeps all count,
        // duplicate and element findings from this same traversal.
        for (index, error) in constraints_errors {
            if let Some(error) = error {
                checked
                    .findings
                    .local_error
                    .get_or_insert_with(|| error.clone());
                checked.findings.local.push((
                    index.map_or_else(|| path.clone(), |i| path.push_index(i)),
                    error,
                ));
            } else if let Some(i) = index
                && let Some(error) = &children[i].findings.local_error
            {
                checked
                    .findings
                    .local_error
                    .get_or_insert_with(|| error.clone().at_list_index(i));
            }
        }
        for child in children {
            checked.findings.local.extend(child.findings.local);
            checked.findings.provider.extend(child.findings.provider);
        }
        checked.fit = checked.findings.local_error.is_none().then(|| {
            Fit::with_kind(
                exactness,
                attr,
                if string_list {
                    FitKind::StringList(fits)
                } else {
                    FitKind::List(fits)
                },
            )
        });
        checked
    }

    fn union<P: PlaceholderKind, B: BindingAddress>(
        &self,
        members: &'a [CompiledAttributeType],
        value: &Value<P, B>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> Result<UnionFit<'a>, Box<ValueValidationFindings>> {
        let mut best: Option<(&CompiledAttributeType, CheckedValue<'a>)> = None;
        let mut failure: Option<BestUnionMemberFailures> = None;
        let mut provider_failure = false;
        let mut provider_only_errors: Option<Vec<(FieldPath, TypeError)>> = None;
        let mut sibling_provider_errors = Vec::new();
        for member in members {
            let checked = self.walk(member, value, path, ancestry, visited);
            if checked.accepts() {
                if best
                    .as_ref()
                    .is_none_or(|(_, current)| checked.fit > current.fit)
                {
                    best = Some((member, checked));
                }
                if best.as_ref().is_some_and(|(_, checked)| {
                    checked
                        .fit
                        .as_ref()
                        .is_some_and(|fit| fit.exactness.is_maximal())
                }) {
                    break;
                }
                continue;
            }
            let Some(concrete) = value.as_concrete() else {
                continue;
            };
            let judgement =
                union_member_judgement_on_path(member, concrete, self.defs, ancestry, visited);
            let Some(error) = checked.findings.first_error().cloned() else {
                continue;
            };
            let is_provider = checked.findings.local_error.is_none();
            if !judgement.failure_rank.is_match() && !is_provider {
                continue;
            }
            let (provider_errors, sibling_errors) = if is_provider {
                (Some(checked.findings.provider), Vec::new())
            } else {
                (None, checked.findings.provider)
            };
            let candidate = UnionMemberFailure {
                error,
                diagnostic_identity: judgement.diagnostic_identity,
            };
            match &mut failure {
                Some(current) if judgement.failure_rank > current.failure_rank => {
                    *current = BestUnionMemberFailures::new(judgement.failure_rank, candidate);
                    provider_failure = is_provider;
                    provider_only_errors = provider_errors;
                    sibling_provider_errors = sibling_errors;
                }
                Some(current) if judgement.failure_rank == current.failure_rank => {
                    current.tied.push(candidate);
                    provider_failure |= is_provider;
                    for finding in sibling_errors {
                        if !sibling_provider_errors.contains(&finding) {
                            sibling_provider_errors.push(finding);
                        }
                    }
                    match (&mut provider_only_errors, provider_errors) {
                        (Some(best), Some(errors)) => {
                            if errors.len() < best.len() {
                                *best = errors;
                            }
                        }
                        _ => provider_only_errors = None,
                    }
                }
                Some(_) => {}
                None => {
                    failure = Some(BestUnionMemberFailures::new(
                        judgement.failure_rank,
                        candidate,
                    ));
                    provider_failure = is_provider;
                    provider_only_errors = provider_errors;
                    sibling_provider_errors = sibling_errors;
                }
            }
        }
        if let Some((member, CheckedValue { fit: Some(fit), .. })) = best {
            return Ok(UnionFit { member, fit });
        }
        // Keep main's complete provider error set when the best shape tier
        // contains only locally accepted arms. Within that tier, preserve its
        // smallest-set/declaration-order choice instead of inventing a new
        // Union message or dropping sibling provider findings.
        if let Some(provider) = provider_only_errors {
            return Err(Box::new(ValueValidationFindings {
                provider,
                ..Default::default()
            }));
        }
        let error = match (failure, value.as_concrete()) {
            (Some(failure), Some(concrete)) => failure.into_error(
                concrete,
                UnionMembers::new_after_union_on_path(members, self.defs, visited.clone()),
                ancestry,
            ),
            _ => TypeError::value_type_mismatch(
                members
                    .iter()
                    .map(CompiledAttributeType::type_name)
                    .collect::<Vec<_>>()
                    .join(" | "),
                value.type_name(),
            ),
        };
        let mut checked = if provider_failure {
            CheckedValue {
                fit: None,
                findings: ValueValidationFindings {
                    provider: vec![(path.clone(), error)],
                    ..Default::default()
                },
            }
        } else {
            CheckedValue::rejected(path, error)
        };
        checked.findings.provider.extend(sibling_provider_errors);
        Err(Box::new(checked.findings))
    }

    fn leaf<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        value: &Value<P, B>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> CheckedValue<'a> {
        let fit = if matches!(attr.kind, AttrTypeKind::Enum { .. }) {
            match fit_value_to_type_on_path(
                attr,
                value,
                self.defs,
                ancestry,
                CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
                visited,
            ) {
                Some(fit) => fit,
                None => {
                    return CheckedValue::rejected(
                        path,
                        attr.validate_leaf_value(value).err().unwrap_or_else(|| {
                            TypeError::value_type_mismatch(attr.type_name(), value.type_name())
                        }),
                    );
                }
            }
        } else {
            if let Err(error) = attr.validate_leaf_value(value) {
                return CheckedValue::rejected(path, error);
            }
            let exactness = if matches!(
                (&attr.kind, value.as_concrete()),
                (AttrTypeKind::Float { .. }, Some(ConcreteValueRef::Int(_)))
            ) {
                FitExactness::COERCIBLE
            } else {
                FitExactness::EXACT
            };
            Fit::with_kind(exactness, attr, FitKind::PassThrough)
        };
        let mut checked = CheckedValue {
            fit: Some(fit),
            findings: Default::default(),
        };
        if let Some(lookup) = self.lookup
            && !has_native_validator(attr)
        {
            match self.canonical_leaf(attr, value, checked.fit.as_ref()) {
                Ok(Some(proof)) => {
                    if proof.requires_provider()
                        && let Err(error) = lookup.validate(proof)
                    {
                        checked.findings.provider.push((path.clone(), error));
                    }
                }
                Ok(None) => {}
                Err(CanonicalLeafError { declaration, error }) => {
                    checked.findings.provider.push((
                        path.clone(),
                        lookup.canonicalization_error(declaration, *error),
                    ));
                }
            }
        }
        checked
    }

    fn canonical_leaf<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        value: &Value<P, B>,
        fit: Option<&Fit<'a>>,
    ) -> Result<Option<LocallyValidatedCustomType<'a>>, CanonicalLeafError<'a>> {
        let Some(declaration) = crate::parser::IdentifiedCustomTypeDeclaration::new(attr) else {
            return Ok(None);
        };
        // Native callbacks already ran after their portable guard; references
        // carry no provider validation capability.
        if matches!(&attr.kind, AttrTypeKind::Reference { .. }) {
            return Ok(None);
        }
        let requires_provider = !has_native_validator(attr);
        let value = if let Some(Fit {
            kind: FitKind::Enum { canonical, .. },
            ..
        }) = fit
        {
            Value::Concrete(ConcreteValue::CanonicalEnum(canonical.clone()))
        } else {
            let Some(value) = value
                .to_expanded()
                .and_then(Value::try_into_address_free_absolute)
            else {
                return Ok(None);
            };
            value
        };
        let canonical =
            crate::provider::canonicalize_custom_type_value(attr, &value).map_err(|error| {
                CanonicalLeafError {
                    declaration,
                    error: Box::new(error.into_type_error()),
                }
            })?;
        Ok(canonical.map(|canonical| LocallyValidatedCustomType {
            declaration,
            value,
            canonical,
            requires_provider,
        }))
    }
}

fn has_native_validator(attr: &CompiledAttributeType) -> bool {
    match &attr.kind {
        AttrTypeKind::String { binding, .. }
        | AttrTypeKind::Int { binding, .. }
        | AttrTypeKind::Float { binding, .. } => binding.callback().is_some(),
        AttrTypeKind::Enum { validate, .. } => validate.is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::CompiledAttributeType as AT;
    use super::*;

    fn text(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }
    #[test]
    fn combined_walk_native_callbacks_run_once_after_local_checks() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let ty = AT::list(AT::refined_string_with_validator(
            TypeIdentity::bare("Short"),
            None,
            Some((None, Some(3))),
            validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(TypeError::ValidationFailed {
                    message: "native rejection".into(),
                })
            }),
            None,
        ));
        let value = Value::Concrete(ConcreteValue::List(vec![text("abcd"), text("bad")]));
        let checked = ValueValidator {
            defs: empty_defs_for_schema_walks(),
            lookup: Some(no_lookup()),
        }
        .walk(
            &ty,
            &value,
            &FieldPath::new(),
            FieldAncestry::WRITABLE,
            &mut Vec::new(),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(checked.findings.local.len(), 2);
        assert!(checked.findings.provider.is_empty());
    }
}

#[cfg(test)]
mod rootless_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn combined_walk_keeps_existing_provider_only_union_diagnostics() {
        for list in [false, true] {
            let member = |name| {
                let leaf = CompiledAttributeType::refined_string(
                    Some(TypeIdentity::bare(name)),
                    None,
                    None,
                    None,
                );
                if list {
                    CompiledAttributeType::list(leaf)
                } else {
                    leaf
                }
            };
            let ty = CompiledAttributeType::union(vec![member("a"), member("b")]);
            let text: Value = Value::Concrete(ConcreteValue::String("bad".into()));
            let value = if list {
                Value::Concrete(ConcreteValue::List(vec![text.clone(), text]))
            } else {
                text
            };
            let lookup = |declaration: &CompiledAttributeType, _: &Value| {
                let identity =
                    crate::provider::identified_custom_type_identity(declaration).unwrap();
                Err(TypeError::ValidationFailed {
                    message: format!("{} rejects", identity.kind).into(),
                })
            };
            let errors = RootlessType::new(ty)
                .validate_with_lookup(&value, &lookup)
                .unwrap_err();
            assert_eq!(
                errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                vec!["Validation failed: a rejects"; if list { 2 } else { 1 }]
            );
        }
    }

    #[test]
    fn combined_walk_rootless_checks_valid_siblings_and_masks_secrets() {
        let ty = CompiledAttributeType::list(CompiledAttributeType::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(3))),
            None,
        ));
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("abcd".into())),
            Value::Concrete(ConcreteValue::String("bad".into())),
        ]));
        for ty in [
            ty.clone(),
            CompiledAttributeType::union(vec![ty, CompiledAttributeType::int()]),
        ] {
            for secret in [false, true] {
                let calls = AtomicUsize::new(0);
                let lookup = |_: &CompiledAttributeType, value: &Value| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(TypeError::ValidationFailed {
                        message: format!("provider rejected {value:?}").into(),
                    })
                };
                let value = if secret {
                    Value::Deferred(DeferredValue::Secret(Box::new(value.clone()).into()))
                } else {
                    value.clone()
                };
                let errors = RootlessType::new(ty.clone())
                    .validate_with_lookup(&value, &lookup)
                    .unwrap_err();
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(errors.len(), 2);
                if secret {
                    let rendered = format!("{errors:?}");
                    assert!(!rendered.contains("abcd"), "{rendered}");
                    assert!(!rendered.contains("bad"), "{rendered}");
                }
            }
        }
    }
}
