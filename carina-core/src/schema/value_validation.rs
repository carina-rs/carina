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

    pub fn into_local_findings(self) -> Vec<(FieldPath, TypeError)> {
        self.local
    }

    pub fn local_error(&self) -> Option<&TypeError> {
        self.local_error.as_ref()
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
    pub fn take_attribute(&mut self, name: &str) -> Option<ValueValidationFindings> {
        self.attributes.remove(name)
    }

    /// The boolean identifies provider findings. Renderers apply their usual
    /// diagnostic deduplication to both local and provider errors.
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

/// Acceptance and diagnostic ranking are separate: a locally rejected value
/// cannot carry a fit that an enclosing Union could mistake for acceptance.
enum LocalOutcome<F> {
    Accepted(F),
    Rejected(TypeError),
}

impl<F> LocalOutcome<F> {
    fn error(&self) -> Option<&TypeError> {
        match self {
            Self::Accepted(_) => None,
            Self::Rejected(error) => Some(error),
        }
    }

    fn fit(&self) -> Option<&F> {
        match self {
            Self::Accepted(fit) => Some(fit),
            Self::Rejected(_) => None,
        }
    }

    fn into_fit(self) -> Option<F> {
        match self {
            Self::Accepted(fit) => Some(fit),
            Self::Rejected(_) => None,
        }
    }

    fn map<G>(self, f: impl FnOnce(F) -> G) -> LocalOutcome<G> {
        match self {
            Self::Accepted(fit) => LocalOutcome::Accepted(f(fit)),
            Self::Rejected(error) => LocalOutcome::Rejected(error),
        }
    }
}

#[derive(Default)]
struct CheckedFindings {
    local: Vec<(FieldPath, TypeError)>,
    provider: Vec<(FieldPath, TypeError)>,
}

impl InsideSecret for CheckedFindings {
    fn inside_secret(self) -> Self {
        Self {
            local: self.local.inside_secret(),
            provider: self.provider.inside_secret(),
        }
    }
}

struct CheckedValue<'a> {
    local: LocalOutcome<Fit<'a>>,
    findings: CheckedFindings,
}

impl InsideSecret for CheckedValue<'_> {
    fn inside_secret(self) -> Self {
        Self {
            local: match self.local {
                LocalOutcome::Accepted(fit) => LocalOutcome::Accepted(fit),
                LocalOutcome::Rejected(error) => LocalOutcome::Rejected(error.inside_secret()),
            },
            findings: self.findings.inside_secret(),
        }
    }
}

struct UnionFailure<'a> {
    local: LocalOutcome<UnionFit<'a>>,
    findings: CheckedFindings,
}

struct UnionProviderFailure<'a> {
    local_fit: UnionFit<'a>,
    diagnostic_rank: UnionFailureRank,
    provider: Vec<(FieldPath, TypeError)>,
}

impl<'a> CheckedValue<'a> {
    fn accepted(
        attr: &'a CompiledAttributeType,
        exactness: FitExactness,
        kind: FitKind<'a>,
    ) -> Self {
        Self {
            local: LocalOutcome::Accepted(Fit::with_kind(exactness, attr, kind)),
            findings: Default::default(),
        }
    }

    fn rejected(path: &FieldPath, error: TypeError) -> Self {
        Self {
            local: LocalOutcome::Rejected(error.clone()),
            findings: CheckedFindings {
                local: vec![(path.clone(), error)],
                provider: Vec::new(),
            },
        }
    }

    fn accepts(&self) -> bool {
        matches!(self.local, LocalOutcome::Accepted(_)) && self.findings.provider.is_empty()
    }

    fn reject(&mut self, error: TypeError) {
        if matches!(self.local, LocalOutcome::Accepted(_)) {
            self.local = LocalOutcome::Rejected(error);
        }
    }

    fn first_error(&self) -> Option<&TypeError> {
        self.local
            .error()
            .or_else(|| self.findings.provider.first().map(|(_, error)| error))
    }

    fn into_findings(self) -> ValueValidationFindings {
        ValueValidationFindings {
            local_error: match self.local {
                LocalOutcome::Accepted(_) => None,
                LocalOutcome::Rejected(error) => Some(error),
            },
            local: self.findings.local,
            provider: self.findings.provider,
        }
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
        .into_findings()
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
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
) -> ValueValidationFindings {
    ValueValidator {
        defs,
        lookup: Some(lookup),
    }
    .walk(attr, value, &FieldPath::new(), ancestry, &mut Vec::new())
    .into_findings()
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
    if let Some(error) = checked.first_error() {
        return Err(error.clone());
    }
    let proof = validator
        .canonical_leaf(
            declaration.declared_type(),
            value,
            checked.local.fit(),
            !has_native_validator(declaration.declared_type()),
        )
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
                if checked.first_error().is_some() {
                    Err(Box::new(checked))
                } else {
                    Ok(checked)
                }
            });
            let checked = checked.unwrap_or_else(|checked| *checked);
            return CheckedValue {
                local: checked
                    .local
                    .map(|fit| Fit::with_kind(fit.exactness, attr, FitKind::Secret(Box::new(fit)))),
                findings: checked.findings,
            };
        }
        if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
            let mut left = self.walk(attr, value, path, ancestry, &mut Vec::new());
            let right = self.walk(attr, default, path, ancestry, &mut Vec::new());
            // Deferred coalesces have one authored parent location. Keep the
            // established primary local diagnostic there, while retaining every
            // provider finding from the independently known branches.
            left.findings.local = left
                .local
                .error()
                .or_else(|| right.local.error())
                .map(|error| vec![(path.clone(), error.clone())])
                .unwrap_or_default();
            left.findings.provider.extend(right.findings.provider);
            left.local = match (left.local, right.local) {
                (LocalOutcome::Accepted(value), LocalOutcome::Accepted(default)) => {
                    let mut exactness = value.exactness;
                    exactness.combine(default.exactness);
                    LocalOutcome::Accepted(Fit::with_kind(
                        exactness,
                        attr,
                        FitKind::Coalesce {
                            value: Box::new(value),
                            default: Box::new(default),
                        },
                    ))
                }
                (LocalOutcome::Rejected(error), _) | (_, LocalOutcome::Rejected(error)) => {
                    LocalOutcome::Rejected(error)
                }
            };
            return left;
        }
        if let Value::Deferred(DeferredValue::ModuleIf(branches)) = value {
            let mut checked = CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Deferred);
            for branch in &branches.values()[1..] {
                let branch = self.walk(attr, branch, path, ancestry, &mut Vec::new());
                // The selected branch is checked after expansion. Until then,
                // preserve main's deferred local outcome while requiring each
                // concrete leaf's local proof before any provider dispatch.
                checked.findings.provider.extend(branch.findings.provider);
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
                Err(failure) => CheckedValue {
                    local: failure.local.map(|selected| {
                        Fit::with_kind(
                            selected.fit.exactness,
                            attr,
                            FitKind::Union {
                                member: selected.member,
                                fit: Box::new(selected.fit),
                            },
                        )
                    }),
                    findings: failure.findings,
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
                    local: LocalOutcome::Accepted(fit),
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
                        checked.reject(error.clone());
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
                    let field = struct_field_for_key(fields, key, ancestry);
                    let child = match field {
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
                    if let Some(error) = child.local.error().cloned() {
                        // Unknown/writability errors are already anchored to their field.
                        let error = if field.is_none() {
                            error
                        } else {
                            error.at_struct_field(key)
                        };
                        checked.reject(error);
                    }
                    checked.findings.local.extend(child.findings.local);
                    checked.findings.provider.extend(child.findings.provider);
                    if let Some(fit) = child.local.into_fit() {
                        exactness.combine(fit.exactness);
                        fits.push(fit);
                    }
                }
                if let Some(group) = groups.first() {
                    checked.reject(TypeError::FieldGroupViolation {
                        path: FieldPath::new(),
                        group: (*group).clone(),
                    });
                }
                checked.local = checked
                    .local
                    .map(|_| Fit::with_kind(exactness, attr, FitKind::Struct(fits)));
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
                    checked.reject(error.clone());
                    checked.findings.local.push((path.clone(), error));
                }
                let mut fits = Vec::new();
                let mut exactness = FitExactness::GENERIC_MAP;
                let mut first_value_error = None;
                for (map_key, map_value) in map {
                    let next_path = path.push_field(map_key.clone());
                    let key_value: Value<P, B> = lift_map_key(key, map_key, self.defs);
                    // Map keys have local constraints, but main dispatches only values.
                    let key_checked = ValueValidator {
                        defs: self.defs,
                        lookup: None,
                    }
                    .walk(
                        key,
                        &key_value,
                        &next_path,
                        ancestry,
                        &mut Vec::new(),
                    );
                    if let Some(error) = key_checked.local.error().cloned() {
                        let error = TypeError::MapKeyError {
                            key: map_key.clone(),
                            inner: Box::new(error),
                        };
                        checked.reject(error.clone());
                        checked.findings.local.push((next_path.clone(), error));
                    }
                    let child =
                        self.walk(value_type, map_value, &next_path, ancestry, &mut Vec::new());
                    let has_coarse_error =
                        child.findings.local.iter().any(|(_, error)| {
                            !matches!(error, TypeError::FieldGroupViolation { .. })
                        });
                    if let Some(error) = child.local.error().cloned() {
                        checked.reject(error.clone().at_map_value(map_key));
                        if has_coarse_error {
                            // The collector's coarse diagnostic always uses
                            // MapValueError, even when the primary inner error
                            // is a field group with its own precise path.
                            first_value_error.get_or_insert(TypeError::MapValueError {
                                key: map_key.clone(),
                                inner: Box::new(error),
                            });
                        }
                    }
                    checked
                        .findings
                        .local
                        .extend(child.findings.local.into_iter().filter(|(_, error)| {
                            matches!(error, TypeError::FieldGroupViolation { .. })
                        }));
                    checked.findings.provider.extend(child.findings.provider);
                    if let (Some(key_fit), Some(value_fit)) =
                        (key_checked.local.into_fit(), child.local.into_fit())
                    {
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
                checked.local = checked
                    .local
                    .map(|_| Fit::with_kind(exactness, attr, FitKind::Map(fits)));
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
        let mut children = Vec::new();
        let mut child_fits = Vec::new();
        for (i, value) in items.iter().enumerate() {
            let child = self.walk(inner, value, &path.push_index(i), ancestry, &mut Vec::new());
            let (fit, error) = match child.local {
                LocalOutcome::Accepted(fit) => (Some(fit), None),
                LocalOutcome::Rejected(error) => (None, Some(error)),
            };
            child_fits.push(fit);
            children.push((error, child.findings));
        }
        let mut child_fits = child_fits.into_iter();
        let mut checked =
            CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::List(Vec::new()));
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
                checked.reject(error.clone());
                checked.findings.local.push((
                    index.map_or_else(|| path.clone(), |i| path.push_index(i)),
                    error,
                ));
            } else if let Some(i) = index
                && let Some(error) = &children[i].0
            {
                checked.reject(error.clone().at_list_index(i));
            }
        }
        for (_, findings) in children {
            checked.findings.local.extend(findings.local);
            checked.findings.provider.extend(findings.provider);
        }
        checked.local = checked.local.map(|_| {
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
    ) -> Result<UnionFit<'a>, Box<UnionFailure<'a>>> {
        let mut best_accepted: Option<UnionFit<'a>> = None;
        let mut failure: Option<BestUnionMemberFailures> = None;
        let mut provider_failure: Option<UnionProviderFailure<'a>> = None;
        let mut sibling_provider_errors = Vec::new();
        for member in members {
            let checked = self.walk(member, value, path, ancestry, visited);
            let accepts = checked.accepts();
            if accepts {
                if let LocalOutcome::Accepted(fit) = checked.local {
                    if fit.exactness.is_maximal() {
                        return Ok(UnionFit { member, fit });
                    }
                    if best_accepted
                        .as_ref()
                        .is_none_or(|current| fit > current.fit)
                    {
                        best_accepted = Some(UnionFit { member, fit });
                    }
                }
                continue;
            }
            let judgement = value
                .as_concrete()
                .map(|concrete| {
                    union_member_judgement_on_path(member, concrete, self.defs, ancestry, visited)
                })
                .unwrap_or_else(UnionMemberJudgement::no_match);
            let error = match checked.local {
                LocalOutcome::Accepted(fit) => {
                    // This is a local fit only. It supports list uniqueness
                    // when every provider rejects, but never Union acceptance.
                    let rank = judgement.failure_rank;
                    match &mut provider_failure {
                        Some(best) => {
                            if fit > best.local_fit.fit {
                                best.local_fit = UnionFit { member, fit };
                            }
                            if rank > best.diagnostic_rank
                                || (rank == best.diagnostic_rank
                                    && checked.findings.provider.len() < best.provider.len())
                            {
                                best.diagnostic_rank = rank;
                                best.provider = checked.findings.provider;
                            }
                        }
                        None => {
                            provider_failure = Some(UnionProviderFailure {
                                local_fit: UnionFit { member, fit },
                                diagnostic_rank: rank,
                                provider: checked.findings.provider,
                            })
                        }
                    }
                    continue;
                }
                LocalOutcome::Rejected(error) => error,
            };
            if !judgement.failure_rank.is_match() {
                continue;
            }
            let candidate = UnionMemberFailure {
                error,
                diagnostic_identity: judgement.diagnostic_identity,
            };
            match &mut failure {
                Some(current) if judgement.failure_rank > current.failure_rank => {
                    *current = BestUnionMemberFailures::new(judgement.failure_rank, candidate);
                    sibling_provider_errors = checked.findings.provider;
                }
                Some(current) if judgement.failure_rank == current.failure_rank => {
                    current.tied.push(candidate);
                    for finding in checked.findings.provider {
                        if !sibling_provider_errors.contains(&finding) {
                            sibling_provider_errors.push(finding);
                        }
                    }
                }
                Some(_) => {}
                None => {
                    failure = Some(BestUnionMemberFailures::new(
                        judgement.failure_rank,
                        candidate,
                    ));
                    sibling_provider_errors = checked.findings.provider;
                }
            }
        }
        if let Some(best) = best_accepted {
            return Ok(best);
        }
        // A locally rejected arm's diagnostic rank cannot replace the
        // provider findings of a locally accepted arm, including at a tie.
        if let Some(failure) = provider_failure {
            return Err(Box::new(UnionFailure {
                local: LocalOutcome::Accepted(failure.local_fit),
                findings: CheckedFindings {
                    provider: failure.provider,
                    ..Default::default()
                },
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
        Err(Box::new(UnionFailure {
            local: LocalOutcome::Rejected(error.clone()),
            findings: CheckedFindings {
                local: vec![(path.clone(), error)],
                provider: sibling_provider_errors,
            },
        }))
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
            local: LocalOutcome::Accepted(fit),
            findings: Default::default(),
        };
        let requires_provider = self.lookup.is_some() && !has_native_validator(attr);
        if let Some(lookup) = self.lookup
            && requires_provider
        {
            match self.canonical_leaf(attr, value, checked.local.fit(), requires_provider) {
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
        requires_provider: bool,
    ) -> Result<Option<LocallyValidatedCustomType<'a>>, CanonicalLeafError<'a>> {
        // Native validation already ran. No dispatch means no value expansion
        // or canonical spelling is needed, including for standalone annotations.
        if !requires_provider {
            return Ok(None);
        }
        let Some(declaration) = crate::parser::IdentifiedCustomTypeDeclaration::new(attr) else {
            return Ok(None);
        };
        // Native callbacks already ran after their portable guard; references
        // carry no provider validation capability.
        if matches!(&attr.kind, AttrTypeKind::Reference { .. }) {
            return Ok(None);
        }
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
    fn issue_4276_round1_nested_union_requires_one_complete_arm() {
        let remote = AT::refined_string(Some(TypeIdentity::bare("VpcId")), None, None, None);
        let defs = BTreeMap::from([(
            "Inner".into(),
            AT::union(vec![
                AT::map(remote),
                AT::struct_(
                    "Named",
                    vec![CompiledStructField::required("name", AT::string()).build()],
                ),
            ]),
        )]);
        let ty = AT::union(vec![AT::ref_("Inner"), AT::bool()]);
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "foo".into(),
            text("bad"),
        )])));
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        let errors =
            validate_value_with_lookup(&ty, &value, &defs, FieldAncestry::WRITABLE, &lookup)
                .into_errors();
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["Validation failed: VpcId rejects bad"]
        );
    }

    #[test]
    fn issue_4276_round1_union_fit_belongs_to_provider_clean_arm() {
        let map = AT::map(AT::string());
        let named = AT::struct_(
            "Named",
            vec![
                CompiledStructField::required(
                    "name",
                    AT::refined_string(Some(TypeIdentity::bare("VpcId")), None, None, None),
                )
                .build(),
            ],
        );
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".into(),
            text("bad"),
        )])));
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        for members in [vec![map.clone(), named.clone()], vec![named, map]] {
            let ty = AT::union(members);
            let checked = ValueValidator {
                defs: empty_defs_for_schema_walks(),
                lookup: Some(&lookup),
            }
            .walk(
                &ty,
                &value,
                &FieldPath::new(),
                FieldAncestry::WRITABLE,
                &mut Vec::new(),
            );
            assert!(checked.accepts());
            assert!(
                matches!(checked.local, LocalOutcome::Accepted(Fit { kind: FitKind::Union { member, .. }, .. }) if matches!(member.kind, AttrTypeKind::Map { .. })),
                "the provider-rejected Struct cannot supply an accepting Union's fit"
            );
        }
    }

    #[test]
    fn issue_4276_round1_union_provider_local_tie_keeps_provider_error() {
        let ty = AT::union(vec![
            AT::refined_string(Some(TypeIdentity::bare("VpcId")), None, None, None),
            AT::refined_string(
                None,
                Some(CompiledPattern::try_new("^x").unwrap()),
                None,
                None,
            ),
        ]);
        let value = text("bad");
        assert!(
            validate_value_with_lookup(
                &ty,
                &value,
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                no_lookup()
            )
            .into_errors()
            .is_empty()
        );
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        let errors = validate_value_with_lookup(
            &ty,
            &value,
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            &lookup,
        )
        .into_errors();
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["Validation failed: VpcId rejects bad"]
        );
    }

    #[test]
    fn issue_4276_round1_module_if_defers_local_errors_and_guards_provider_dispatch() {
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(3))),
            None,
        );
        let value = Value::Deferred(DeferredValue::ModuleIf(Box::new(
            crate::parser::DeferredValueIf::new(
                crate::parser::DeferredIfCondition::new(
                    Value::Concrete(ConcreteValue::Bool(true)),
                    "module/main.crn".into(),
                    2,
                ),
                Value::Concrete(ConcreteValue::String("ok".into())),
                Value::Concrete(ConcreteValue::String("too long".into())),
            ),
        )));
        let calls = std::sync::Mutex::new(Vec::new());
        let lookup = |_: &AT, value: &Value| {
            calls.lock().unwrap().push(value.clone());
            Ok(())
        };
        let findings = validate_value_with_lookup(
            &ty,
            &value,
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            &lookup,
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![text("ok")],
            "a locally invalid branch must never reach a provider"
        );
        assert!(
            findings.local.is_empty(),
            "undecided branches must not produce local findings: {:?}",
            findings.local
        );
        assert!(findings.into_errors().is_empty());
    }

    #[test]
    fn issue_4276_round1_map_field_group_collector_keeps_map_value_wrapper() {
        let choice = AT::struct_with_field_groups(
            "Choice",
            vec![
                CompiledStructField::optional("first", AT::string()).build(),
                CompiledStructField::optional("second", AT::string()).build(),
            ],
            vec![FieldGroup::exactly_one(["first", "second"]).unwrap()],
        );
        let ty = AT::map(AT::struct_(
            "Entry",
            vec![
                CompiledStructField::optional("a", choice).build(),
                CompiledStructField::optional("b", AT::int()).build(),
            ],
        ));
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "entry".into(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "a".into(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                ),
                ("b".into(), text("bad")),
            ]))),
        )])));
        let findings =
            validate_value_locally(&ty, &value, &BTreeMap::new(), FieldAncestry::WRITABLE);
        assert_eq!(findings.local.len(), 2);
        assert_eq!(findings.local[0].0.to_string(), "entry.a");
        assert!(
            matches!(&findings.local[1].1, TypeError::MapValueError { key, inner }
            if key == "entry" && matches!(inner.as_ref(), TypeError::FieldGroupViolation { path, .. } if path.to_string() == "a")),
            "{:?}",
            findings.local
        );
        assert_eq!(
            findings.local_error.unwrap().to_string(),
            "entry.a: exactly one of [first, second] must be set"
        );
    }

    #[test]
    fn issue_4276_round1_provider_dispatch_accepts_struct_block_alias() {
        let ty = AT::struct_(
            "Settings",
            vec![
                CompiledStructField::optional(
                    "entries",
                    AT::list(AT::refined_string(
                        Some(TypeIdentity::bare("VpcId")),
                        None,
                        None,
                        None,
                    )),
                )
                .with_block_name("entry")
                .build(),
            ],
        );
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "entry".into(),
            Value::Concrete(ConcreteValue::List(vec![text("bad")])),
        )])));
        let calls = std::sync::Mutex::new(Vec::new());
        let lookup = |_: &AT, value: &Value| {
            calls.lock().unwrap().push(value.clone());
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        let findings = validate_value_with_lookup(
            &ty,
            &value,
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            &lookup,
        );
        assert_eq!(*calls.lock().unwrap(), vec![text("bad")]);
        assert!(findings.local_error.is_none());
        assert_eq!(findings.provider.len(), 1);
        assert_eq!(findings.provider[0].0.to_string(), "entry[0]");
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
    fn issue_4276_round1_rootless_returns_only_primary_local_error() {
        let ty = RootlessType::new(CompiledAttributeType::list(
            CompiledAttributeType::refined_string(
                Some(TypeIdentity::bare("Short")),
                None,
                Some((None, Some(3))),
                None,
            ),
        ));
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("abcd".into())),
            Value::Concrete(ConcreteValue::String("bad".into())),
        ]));
        let local = ty.validate(&value).unwrap_err();
        let lookup = |_: &CompiledAttributeType, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "provider rejected sibling".into(),
            })
        };
        assert_eq!(ty.validate_with_lookup(&value, &lookup), Err(vec![local]));
    }

    #[test]
    fn issue_4276_round1_resource_keeps_unique_list_and_sibling_provider_errors() {
        let schema = PatternResourceSchema::new("test.List")
            .attribute(
                PatternAttributeSchema::optional(
                    "values",
                    PatternAttributeType::refined_list(
                        PatternAttributeType::union(vec![
                            PatternAttributeType::refined_string(
                                Some(TypeIdentity::bare("Remote")),
                                None,
                                None,
                                None,
                            ),
                            PatternAttributeType::refined_string(
                                Some(TypeIdentity::bare("OtherRemote")),
                                None,
                                None,
                                None,
                            ),
                        ]),
                        true,
                        None,
                        ListElements::Unique,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("same".into())),
            Value::Concrete(ConcreteValue::String("same".into())),
        ]));
        let lookup = |_: &CompiledAttributeType, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "provider rejected sibling".into(),
            })
        };
        let errors = schema
            .validate_with_origins_and_lookup(
                &HashMap::from([("values".into(), value)]),
                &|_| false,
                &lookup,
            )
            .unwrap_err();
        assert_eq!(
            errors.len(),
            3,
            "resource findings must retain both provider failures: {errors:#?}"
        );
        assert!(
            matches!(&errors[0], TypeError::DuplicateListElement { index: 1, first_index: 0, attribute: Some(attribute), .. } if attribute == "values")
        );
        for error in &errors[1..] {
            assert_eq!(error.attribute_name(), Some("values"));
            assert_eq!(
                error.to_string(),
                "Resource validation failed: provider rejected sibling"
            );
        }
    }

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
                assert_eq!(errors.len(), 1);
                if secret {
                    let rendered = format!("{errors:?}");
                    assert!(!rendered.contains("abcd"), "{rendered}");
                    assert!(!rendered.contains("bad"), "{rendered}");
                }
            }
        }
    }
}
