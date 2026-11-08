//! Local checks and provider dispatch share a value traversal and Union choice.

use super::*;
use crate::resource::knowledge::{KnownView, ShapeView};

/// Proof that this exact declaration accepted the value, including its canonical
/// scalar spelling. Only the value validator can construct this proof.
pub struct LocallyValidatedCustomType<'a> {
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: Value,
    canonical: crate::provider::CanonicalCustomTypeValue,
}

impl LocallyValidatedCustomType<'_> {
    pub fn declaration(&self) -> crate::parser::IdentifiedCustomTypeDeclaration<'_> {
        self.declaration
    }

    pub fn value(&self) -> &Value {
        &self.value
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

/// Outer structure recorded by the classified validation walk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ValueStructure {
    #[default]
    Scalar,
    List,
    Map,
    Protected,
    Pending,
    Failed,
}

fn value_structure<P: PlaceholderKind, B: BindingAddress, T>(
    value: &Classification<'_, P, B, T>,
) -> ValueStructure {
    match value {
        Classification::Known(value) => value.visit(
            |view| match view {
                KnownView::List(_) | KnownView::StringList(_) => ValueStructure::List,
                KnownView::Map(_) => ValueStructure::Map,
                KnownView::String(_)
                | KnownView::EnumIdentifier(_)
                | KnownView::CanonicalEnum(_)
                | KnownView::Int(_)
                | KnownView::Float(_)
                | KnownView::Bool(_)
                | KnownView::Duration(_) => ValueStructure::Scalar,
            },
            |_| ValueStructure::Protected,
        ),
        Classification::ShapeKnown(value) => value.visit(
            |view| match view {
                ShapeView::List(_) => ValueStructure::List,
                ShapeView::Map(_) => ValueStructure::Map,
            },
            |_| ValueStructure::Protected,
        ),
        Classification::Pending(_) => ValueStructure::Pending,
        Classification::Failed(_) => ValueStructure::Failed,
    }
}

/// Cached findings for one attribute. The primary error retains the CLI's
/// established nesting; paths retain the LSP's established element locations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValueValidationFindings {
    structure: ValueStructure,
    local_error: Option<TypeError>,
    additional_local_errors: Vec<TypeError>,
    local: Vec<(FieldPath, TypeError)>,
    located: Vec<(LeafPath, TypeError)>,
    assignment: AssignmentFindings,
    pub(crate) symbolic: Vec<(LeafPath, TypeError)>,
    pub(super) provider: Vec<(FieldPath, TypeError)>,
}

impl ValueValidationFindings {
    pub(crate) fn take_assignment(&mut self) -> AssignmentFindings {
        std::mem::take(&mut self.assignment)
    }

    pub(crate) fn into_located(self) -> Vec<(LeafPath, TypeError)> {
        self.located
            .into_iter()
            .chain(
                self.provider
                    .into_iter()
                    .map(|(path, error)| (path.canonical(), error)),
            )
            .collect()
    }

    pub fn structure(&self) -> ValueStructure {
        self.structure
    }

    pub(super) fn into_local_result(self) -> Result<(), TypeError> {
        self.local_error.map_or(Ok(()), Err)
    }

    pub fn into_local_findings(self) -> Vec<(FieldPath, TypeError)> {
        self.local
    }

    pub fn symbolic_errors(&self) -> &[(LeafPath, TypeError)] {
        &self.symbolic
    }

    pub fn local_findings(&self) -> &[(FieldPath, TypeError)] {
        &self.local
    }

    pub fn local_error(&self) -> Option<&TypeError> {
        self.local_error.as_ref()
    }

    pub(crate) fn local_errors(&self) -> impl Iterator<Item = &TypeError> {
        self.local_error.iter().chain(&self.additional_local_errors)
    }

    pub(super) fn into_errors(self) -> Vec<TypeError> {
        self.local_error
            .into_iter()
            .chain(self.additional_local_errors)
            .chain(self.provider.into_iter().map(|(_, error)| error))
            .collect()
    }
}

impl InsideSecret for ValueValidationFindings {
    fn inside_secret(self) -> Self {
        Self {
            structure: ValueStructure::Protected,
            local_error: self.local_error.map(InsideSecret::inside_secret),
            additional_local_errors: self.additional_local_errors.inside_secret(),
            local: self.local.inside_secret(),
            located: self
                .located
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
            assignment: self.assignment.inside_secret(),
            symbolic: self
                .symbolic
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
            provider: self.provider.inside_secret(),
        }
    }
}

/// One validation report, usable by both CLI and LSP without revalidation.
#[derive(Default)]
pub struct ResourceValueValidation {
    pub(super) errors: Vec<TypeError>,
    pub(super) attributes: HashMap<String, ValueValidationFindings>,
}

impl ResourceValueValidation {
    /// Render a destination once, using the location retained by the shared
    /// walk. The original TypeError remains available through errors().
    pub fn diagnostic_error(&self, error: &TypeError) -> TypeError {
        let Some(attribute) = error.attribute_name() else {
            return error.clone();
        };
        let Some(findings) = self.attributes.get(attribute) else {
            return error.clone();
        };
        let path = if findings
            .local_error
            .as_ref()
            .is_some_and(|primary| primary.clone().with_attribute(attribute) == *error)
        {
            findings.located.first().map(|(path, _)| path.clone())
        } else {
            findings
                .provider
                .iter()
                .find(|(_, finding)| finding.clone().with_attribute(attribute) == *error)
                .map(|(path, _)| path.canonical())
        };
        match path {
            // List and map wrappers already identify their elements and keep
            // the established top-level attribute label. Struct paths use the
            // canonical dotted destination from the same recorded location.
            Some(path)
                if !path.segments().is_empty()
                    && path.segments().iter().all(|segment| {
                        matches!(segment, super::LeafPathSegment::StructField(_))
                    }) =>
            {
                let path = LeafPath::from_segments(
                    std::iter::once(super::LeafPathSegment::StructField(attribute.into()))
                        .chain(path.segments().iter().cloned()),
                );
                error.clone().with_attribute(path.to_string())
            }
            Some(_) | None => error.clone(),
        }
    }

    pub fn take_attribute(&mut self, name: &str) -> Option<ValueValidationFindings> {
        self.attributes.remove(name)
    }

    pub fn symbolic_errors(&self) -> impl Iterator<Item = (&str, &LeafPath, &TypeError)> {
        self.attributes.iter().flat_map(|(name, findings)| {
            findings
                .symbolic
                .iter()
                .map(move |(path, error)| (name.as_str(), path, error))
        })
    }

    pub fn errors(&self) -> impl Iterator<Item = &TypeError> {
        self.errors.iter()
    }

    pub(crate) fn into_errors(self) -> Vec<TypeError> {
        self.errors
    }

    pub(crate) fn into_result(self) -> Result<(), Vec<TypeError>> {
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
    Rejected { primary: (FieldPath, TypeError) },
}

impl<F> LocalOutcome<F> {
    fn error(&self) -> Option<&TypeError> {
        match self {
            Self::Accepted(_) => None,
            Self::Rejected {
                primary: (_, error),
            } => Some(error),
        }
    }

    fn fit(&self) -> Option<&F> {
        match self {
            Self::Accepted(fit) => Some(fit),
            Self::Rejected { .. } => None,
        }
    }

    fn into_fit(self) -> Option<F> {
        match self {
            Self::Accepted(fit) => Some(fit),
            Self::Rejected { .. } => None,
        }
    }

    fn map<G>(self, f: impl FnOnce(F) -> G) -> LocalOutcome<G> {
        match self {
            Self::Accepted(fit) => LocalOutcome::Accepted(f(fit)),
            Self::Rejected { primary } => LocalOutcome::Rejected { primary },
        }
    }
}

#[derive(Default)]
struct CheckedFindings {
    local: Vec<(FieldPath, TypeError)>,
    located: Vec<(LeafPath, TypeError)>,
    assignment: AssignmentFindings,
    // Other undecided branches have independent primary local errors. Ordinary
    // containers still retain their established first-error CLI precedence.
    additional_local_errors: Vec<TypeError>,
    provider: Vec<(FieldPath, TypeError)>,
}

impl CheckedFindings {
    fn push_local(&mut self, path: FieldPath, error: TypeError) {
        self.located.push((path.canonical(), error.clone()));
        self.local.push((path, error));
    }
}

impl InsideSecret for CheckedFindings {
    fn inside_secret(self) -> Self {
        Self {
            local: self.local.inside_secret(),
            located: self
                .located
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
            assignment: self.assignment.inside_secret(),
            additional_local_errors: self.additional_local_errors.inside_secret(),
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
                LocalOutcome::Rejected { primary } => LocalOutcome::Rejected {
                    primary: primary.inside_secret(),
                },
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
        let primary = (path.clone(), error);
        Self {
            local: LocalOutcome::Rejected {
                primary: primary.clone(),
            },
            findings: CheckedFindings {
                located: vec![(path.canonical(), primary.1.clone())],
                local: vec![primary],
                ..Default::default()
            },
        }
    }

    fn accepts(&self) -> bool {
        matches!(self.local, LocalOutcome::Accepted(_)) && self.findings.provider.is_empty()
    }

    fn reject(&mut self, path: &FieldPath, error: TypeError) {
        if matches!(self.local, LocalOutcome::Accepted(_)) {
            self.local = LocalOutcome::Rejected {
                primary: (path.clone(), error),
            };
        }
    }

    /// Container context applies equally to every undecided branch's CLI
    /// error. Path-tagged LSP findings keep their precise child locations.
    fn wrap_local_errors(mut self, wrap: impl Fn(TypeError) -> TypeError) -> Self {
        if let LocalOutcome::Rejected {
            primary: (_, error),
        } = &mut self.local
        {
            *error = wrap(error.clone());
        }
        self.findings.additional_local_errors = self
            .findings
            .additional_local_errors
            .into_iter()
            .map(wrap)
            .collect();
        self
    }

    fn retain_primary_finding(&mut self) {
        if let LocalOutcome::Rejected { primary } = &self.local
            && self.findings.local.is_empty()
        {
            // Containers aggregate primary errors separately from their
            // precise child findings. Supply a fallback only when no child
            // or container constraint has already supplied a finding.
            self.findings.local.push(primary.clone());
        }
        if let LocalOutcome::Rejected { primary } = &self.local
            && self.findings.located.is_empty()
        {
            self.findings
                .located
                .push((primary.0.canonical(), primary.1.clone()));
        }
    }

    fn first_error(&self) -> Option<&TypeError> {
        self.local
            .error()
            .or_else(|| self.findings.provider.first().map(|(_, error)| error))
    }

    fn into_findings(mut self) -> ValueValidationFindings {
        self.retain_primary_finding();
        ValueValidationFindings {
            structure: ValueStructure::Scalar,
            local_error: match self.local {
                LocalOutcome::Accepted(_) => None,
                LocalOutcome::Rejected {
                    primary: (_, error),
                } => Some(error),
            },
            local: self.findings.local,
            located: self.findings.located,
            assignment: self.findings.assignment,
            symbolic: Vec::new(),
            additional_local_errors: self.findings.additional_local_errors,
            provider: self.findings.provider,
        }
    }
}

struct ValueValidator<'a> {
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    lookup: Option<CustomTypeLookup<'a>>,
}

pub(crate) fn untyped_symbolic_errors(errors: Vec<TypeError>) -> ValueValidationFindings {
    let mut findings = ValueValidationFindings::default();
    for error in errors {
        findings
            .symbolic
            .push((LeafPath::from_segments([]), error.clone()));
        findings.local.push((FieldPath::new(), error.clone()));
        findings
            .located
            .push((LeafPath::from_segments([]), error.clone()));
        if findings.local_error.is_none() {
            findings.local_error = Some(error);
        } else {
            findings.additional_local_errors.push(error);
        }
    }
    findings
}

pub(crate) fn classification_error(error: TypeError) -> ValueValidationFindings {
    CheckedValue::rejected(&FieldPath::new(), error).into_findings()
}
pub(super) fn validate_value_locally<
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
) -> ValueValidationFindings {
    let mut findings = ValueValidator { defs, lookup: None }
        .walk(
            attr,
            value.into(),
            &FieldPath::new(),
            ancestry,
            &mut Vec::new(),
        )
        .into_findings();
    findings.structure = value_structure(value);
    findings
}

#[cfg(test)]
pub(super) fn select_union_member_on_path<'a>(
    members: &'a [CompiledAttributeType],
    value: &Value,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    visited: &mut Vec<&'a str>,
) -> Option<&'a CompiledAttributeType> {
    let knowledge = value
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .expect("evaluable union fixture");
    ValueValidator { defs, lookup: None }
        .union(
            members,
            (&knowledge).into(),
            &FieldPath::new(),
            FieldAncestry::WRITABLE,
            visited,
        )
        .ok()
        .map(|selected| selected.member)
}

pub(crate) fn validate_value_with_lookup<
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
) -> ValueValidationFindings {
    let mut findings = ValueValidator {
        defs,
        lookup: Some(lookup),
    }
    .walk(
        attr,
        value.into(),
        &FieldPath::new(),
        ancestry,
        &mut Vec::new(),
    )
    .into_findings();
    findings.structure = value_structure(value);
    findings
}

/// Standalone declarations enter the same leaf validator as resource values.
/// The continuation receives the sealed proof and cannot be called on a failed
/// local check. This also gives annotations their local declaration guard.
pub(crate) fn validate_custom_type_leaf<'a, R>(
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: &KnownValue<'_>,
    validated: impl FnOnce(Option<LocallyValidatedCustomType<'a>>) -> R,
) -> Result<R, TypeError> {
    let validator = ValueValidator {
        defs: empty_defs_for_schema_walks(),
        lookup: None,
    };
    let checked = validator.walk(
        declaration.declared_type(),
        ClassifiedRef::<crate::resource::NoPlaceholder,crate::resource::AbsoluteBindingAddress>::Known(value),
        &FieldPath::new(),
        FieldAncestry::WRITABLE,
        &mut Vec::new(),
    );
    if let Some(error) = checked.first_error() {
        return Err(error.clone());
    }
    let proof = validator
        .canonical_leaf(declaration.declared_type(), value, checked.local.fit())
        .map_err(|failure| *failure.error)?;
    Ok(validated(proof))
}

/// Annotation declarations check the original scalar before canonicalization.
/// In particular, enumerated strings and numeric enum identifiers must retain
/// their local shape errors instead of being decoded as provider bridge text.
pub(crate) fn validate_custom_type_annotation_leaf<'a, R>(
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    value: &KnownValue<'_>,
    validated: impl FnOnce(Option<LocallyValidatedCustomType<'a>>) -> R,
) -> Result<R, TypeError> {
    declaration
        .declared_type()
        .validate_leaf_value(&value.to_value())?;
    let validator = ValueValidator {
        defs: empty_defs_for_schema_walks(),
        lookup: None,
    };
    let proof = validator
        .canonical_leaf(declaration.declared_type(), value, None)
        .map_err(|failure| *failure.error)?;
    Ok(validated(proof))
}

struct CanonicalLeafError<'a> {
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'a>,
    error: Box<TypeError>,
}

/// An admission callback participates in this traversal and its Union choice.
/// It never classifies a value or walks children.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AssignmentFindings {
    pub(crate) local: Vec<LeafPath>,
    pub(crate) schema: crate::validation::admission::SchemaObligation,
    pub(crate) errors: Vec<(LeafPath, TypeError)>,
}
impl AssignmentFindings {
    fn extend(&mut self, other: Self) {
        self.local.extend(other.local);
        self.schema.extend(other.schema);
        self.errors.extend(other.errors);
    }
}
impl InsideSecret for AssignmentFindings {
    fn inside_secret(self) -> Self {
        Self {
            local: self
                .local
                .into_iter()
                .map(InsideSecret::inside_secret)
                .collect(),
            schema: self.schema,
            errors: self
                .errors
                .into_iter()
                .map(|(path, error)| (path.inside_secret(), error.inside_secret()))
                .collect(),
        }
    }
}
pub(crate) type AssignmentCheck<'a, P, B> = Option<
    &'a dyn Fn(
        &CompiledAttributeType,
        &Value<P, B>,
        &LeafPath,
    ) -> Result<AssignmentFindings, TypeError>,
>;

pub(crate) type AttributeAdmission<'a, 'v, P, B, T> =
    &'a dyn Fn(&str, &Classification<'v, P, B, T>) -> ValueValidationFindings;

struct AdmittedUnion<'a> {
    selected: UnionFit<'a>,
    assignment: AssignmentFindings,
}

pub(crate) fn validate_value_for_admission<
    'v,
    P: PlaceholderKind,
    B: BindingAddress,
    T: StructuralPending<'v, P, B>,
>(
    attr: &CompiledAttributeType,
    value: &Classification<'v, P, B, T>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    lookup: CustomTypeLookup<'_>,
    assignment: AssignmentCheck<'_, P, B>,
) -> ValueValidationFindings {
    let mut checked = ValueValidator {
        defs,
        lookup: Some(lookup),
    }
    .walk_with_assignment(
        attr,
        value.into(),
        &FieldPath::new(),
        ancestry,
        &mut Vec::new(),
        assignment,
    )
    .into_findings();
    checked.structure = value_structure(value);
    checked
}

impl<'a> ValueValidator<'a> {
    fn walk<'v, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'v, P, B>>(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> CheckedValue<'a> {
        self.walk_with_assignment(attr, value, path, ancestry, visited, None)
    }

    #[cfg(test)]
    fn union<'v, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'v, P, B>>(
        &self,
        members: &'a [CompiledAttributeType],
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> Result<UnionFit<'a>, Box<UnionFailure<'a>>> {
        self.union_with_assignment(members, value, path, ancestry, visited, None)
            .map(|admitted| admitted.selected)
    }

    fn walk_with_assignment<
        'v,
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
        assignment: AssignmentCheck<'_, P, B>,
    ) -> CheckedValue<'a> {
        let mut checked = self.walk_value(attr, value, path, ancestry, visited, assignment);
        checked.retain_primary_finding();
        checked
    }

    fn walk_value<'v, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'v, P, B>>(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: AssignmentCheck<'_, P, B>,
    ) -> CheckedValue<'a> {
        if let Some(inner) = value.secret() {
            // The proof stays inside this boundary. Only masked findings leave.
            let checked = inner.try_project_plaintext(|value| {
                let knowledge = value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .map_err(|error| {
                        Box::new(CheckedValue::rejected(path, TypeError::from(error)))
                    })?;
                let checked = self.walk_with_assignment(
                    attr,
                    (&knowledge).into(),
                    path,
                    ancestry.inside_secret(),
                    visited,
                    assignment,
                );
                if checked.first_error().is_some() {
                    Err(Box::new(checked))
                } else {
                    Ok(checked)
                }
            });
            let mut checked = checked.unwrap_or_else(|checked| *checked);
            // Successful symbolic checks also carry protected location metadata.
            checked.findings.assignment = checked.findings.assignment.inside_secret();
            return CheckedValue {
                local: checked
                    .local
                    .map(|fit| Fit::with_kind(fit.exactness, attr, FitKind::Secret(Box::new(fit)))),
                findings: checked.findings,
            };
        }
        if let Some((left_value, default)) = value.coalesce() {
            // A coalesce has an authored assignment of its own. Retain that
            // judgement while the existing walk checks both classified
            // alternatives, including their provider callbacks.
            let parent_assignment = match assignment {
                Some(check) => check(attr, value.diagnostic_value(), &path.canonical()),
                None => Ok(AssignmentFindings::default()),
            };
            let mut left = self.walk_with_assignment(
                attr,
                left_value,
                path,
                ancestry,
                &mut Vec::new(),
                assignment,
            );
            let right = self.walk_with_assignment(
                attr,
                default,
                path,
                ancestry,
                &mut Vec::new(),
                assignment,
            );
            // Deferred coalesces have one authored parent location. Keep the
            // established primary local diagnostic there, while retaining every
            // provider finding from the independently known branches.
            left.findings.local = left
                .local
                .error()
                .or_else(|| right.local.error())
                .map(|error| vec![(path.clone(), error.clone())])
                .unwrap_or_default();
            left.findings.assignment.extend(right.findings.assignment);
            left.findings.located.extend(right.findings.located);
            left.findings.provider.extend(right.findings.provider);
            left.findings
                .additional_local_errors
                .extend(right.findings.additional_local_errors);
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
                (LocalOutcome::Rejected { primary }, _)
                | (_, LocalOutcome::Rejected { primary }) => LocalOutcome::Rejected { primary },
            };
            match parent_assignment {
                Ok(findings) => left.findings.assignment.extend(findings),
                // A branch's local error is the established primary diagnostic
                // at this authored coalesce. Do not add its inferred echo.
                Err(_) if left.local.error().is_some() => {}
                Err(error) => {
                    left.reject(path, error.clone());
                    left.findings
                        .assignment
                        .errors
                        .push((path.canonical(), error));
                }
            }
            return left;
        }
        if let Some(branches) = value.branches() {
            let ClassifiedRef::Pending(pending) = value else {
                unreachable!("only an undecided conditional has branches")
            };
            let mut checked =
                CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Deferred(pending.fit()));
            for branch in branches {
                let branch = self.walk_with_assignment(
                    attr,
                    branch,
                    path,
                    ancestry,
                    &mut Vec::new(),
                    assignment,
                );
                // Expansion removes decided ModuleIf values. Every branch
                // still present here is independently checked in both stages.
                if let Some(error) = branch.local.error() {
                    if checked.local.error().is_some() {
                        checked.findings.additional_local_errors.push(error.clone());
                    } else {
                        checked.reject(path, error.clone());
                    }
                }
                checked
                    .findings
                    .assignment
                    .extend(branch.findings.assignment);
                checked.findings.located.extend(branch.findings.located);
                checked.findings.local.extend(branch.findings.local);
                checked
                    .findings
                    .additional_local_errors
                    .extend(branch.findings.additional_local_errors);
                checked.findings.provider.extend(branch.findings.provider);
            }
            return checked;
        }
        let resolution = match resolve_refs_on_path(attr, self.defs, visited) {
            Ok(resolution) => resolution,
            Err(error) => return CheckedValue::rejected(path, error.into_validation_error()),
        };
        let checked =
            self.walk_resolved(resolution.attr, value, path, ancestry, visited, assignment);
        visited.truncate(resolution.restore_len);
        checked
    }

    fn walk_resolved<'v, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'v, P, B>>(
        &self,
        attr: &'a CompiledAttributeType,
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: AssignmentCheck<'_, P, B>,
    ) -> CheckedValue<'a> {
        let findings = match assignment {
            Some(check) => match check(attr, value.diagnostic_value(), &path.canonical()) {
                Ok(findings) => findings,
                Err(error) => {
                    let mut checked = CheckedValue::rejected(path, error.clone());
                    checked
                        .findings
                        .assignment
                        .errors
                        .push((path.canonical(), error));
                    return checked;
                }
            },
            None => AssignmentFindings::default(),
        };
        let mut checked =
            self.walk_resolved_value(attr, value, path, ancestry, visited, assignment);
        checked.findings.assignment.extend(findings);
        checked
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_resolved_value<
        'v,
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
        assignment: AssignmentCheck<'_, P, B>,
    ) -> CheckedValue<'a> {
        if let AttrTypeKind::UnloadedSchemaType(_) = &attr.kind {
            return CheckedValue::rejected(
                path,
                crate::parser::CustomTypeValidationError::ProvidersNotLoaded.into(),
            );
        }
        match value {
            ClassifiedRef::Failed(_) => {
                return CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Failed);
            }
            ClassifiedRef::Known(_) | ClassifiedRef::Shape(_) | ClassifiedRef::Pending(_) => {}
        }
        if let ClassifiedRef::Pending(pending) = value
            && !pending.arithmetic()
        {
            // An unresolved value has no arm to select yet. In particular,
            // unavailable member schemas cannot reject a deferred Union.
            return CheckedValue::accepted(
                attr,
                FitExactness::EXACT,
                FitKind::Deferred(pending.fit()),
            );
        }
        if let AttrTypeKind::Union(members) = &attr.kind {
            return match self.union_with_assignment(
                members.as_raw_slice(),
                value,
                path,
                ancestry,
                visited,
                assignment,
            ) {
                Ok(admitted) => {
                    let mut checked = CheckedValue::accepted(
                        attr,
                        admitted.selected.fit.exactness,
                        FitKind::Union {
                            member: admitted.selected.member,
                            fit: Box::new(admitted.selected.fit),
                        },
                    );
                    checked.findings.assignment = admitted.assignment;
                    checked
                }
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
        let Some(concrete) = value.concrete() else {
            // Arithmetic has a known numeric result shape even while deferred.
            let fit = inspect_structure_with_defs_on_path(
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
                ConcreteValueRef::List(_items),
            ) => self.list(
                attr,
                element_type,
                *constraints,
                &value.list().expect("classified list membership"),
                false,
                path,
                ancestry,
                assignment,
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
                    .collect::<Vec<Value>>();
                let classified = match items
                    .iter()
                    .map(|value| {
                        value
                            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                            .map(|proof| proof.into_classification())
                    })
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(classified) => classified,
                    Err(error) => return CheckedValue::rejected(path, error.into()),
                };
                let cached = classified
                    .iter()
                    .map(ClassifiedRef::from)
                    .collect::<Vec<_>>();
                // The provider walk visits ordinary Lists only. State and
                // provider StringLists retain their local element checks.
                ValueValidator {
                    defs: self.defs,
                    lookup: None,
                }
                .list(
                    attr,
                    element_type,
                    *constraints,
                    &cached,
                    true,
                    path,
                    ancestry,
                    None,
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
                        checked.reject(path, error.clone());
                        checked
                            .findings
                            .push_local(path.push_field(field.name.clone()), error);
                    }
                }
                let groups = violated_struct_field_groups(fields, field_groups, map, ancestry)
                    .collect::<Vec<_>>();
                for group in &groups {
                    checked.findings.push_local(
                        path.clone(),
                        TypeError::FieldGroupViolation {
                            path: path.clone(),
                            group: (*group).clone(),
                        },
                    );
                }
                let known = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>();
                let mut fits = Vec::new();
                let mut exactness = FitExactness::EXACT;
                let cached = value.map().expect("classified map membership");
                for (key, _value) in map {
                    let value = cached[key.as_str()];
                    let child_path = path.push_field(key.clone());
                    let field = struct_field_for_key(fields, key, ancestry);
                    let child = match field {
                        Some(field) => match validate_struct_field_assignment(field, key, ancestry)
                        {
                            Some(error) => CheckedValue::rejected(&child_path, error),
                            None => self.walk_with_assignment(
                                &field.field_type,
                                value,
                                &child_path,
                                ancestry.descend(field),
                                &mut Vec::new(),
                                assignment,
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
                    let child = child.wrap_local_errors(|error| {
                        // Unknown/writability errors are already anchored to their field.
                        if field.is_none() {
                            error
                        } else {
                            error.at_struct_field(key)
                        }
                    });
                    if let Some(error) = child.local.error().cloned() {
                        checked.reject(path, error);
                    }
                    checked
                        .findings
                        .assignment
                        .extend(child.findings.assignment);
                    checked.findings.located.extend(child.findings.located);
                    checked.findings.local.extend(child.findings.local);
                    checked
                        .findings
                        .additional_local_errors
                        .extend(child.findings.additional_local_errors);
                    checked.findings.provider.extend(child.findings.provider);
                    if let Some(fit) = child.local.into_fit() {
                        exactness.combine(fit.exactness);
                        fits.push(fit);
                    }
                }
                if let Some(group) = groups.first() {
                    checked.reject(
                        path,
                        TypeError::FieldGroupViolation {
                            path: FieldPath::new(),
                            group: (*group).clone(),
                        },
                    );
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
                    checked.reject(path, error.clone());
                    checked.findings.push_local(path.clone(), error);
                }
                let mut fits = Vec::new();
                let mut exactness = FitExactness::GENERIC_MAP;
                let mut first_value_error = None;
                let mut additional_value_errors = Vec::new();
                let cached = value.map().expect("classified map membership");
                for (map_key, _map_value) in map {
                    let map_value = cached[map_key.as_str()];
                    let next_path = path.push_map_key(map_key.clone());
                    let key_value: Value = lift_map_key(key, map_key, self.defs);
                    let key_proof = match key_value
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .map(|proof| proof.into_classification())
                    {
                        Ok(proof) => proof,
                        Err(error) => return CheckedValue::rejected(path, error.into()),
                    };
                    // Map keys have local constraints, but main dispatches only values.
                    let key_checked = ValueValidator {
                        defs: self.defs,
                        lookup: None,
                    }
                    .walk_with_assignment(
                        key,
                        (&key_proof).into(),
                        &next_path,
                        ancestry,
                        &mut Vec::new(),
                        None,
                    );
                    if let Some(error) = key_checked.local.error().cloned() {
                        let error = TypeError::MapKeyError {
                            key: map_key.clone(),
                            inner: Box::new(error),
                        };
                        checked.reject(path, error.clone());
                        checked.findings.push_local(next_path.clone(), error);
                    }
                    let child = self.walk_with_assignment(
                        value_type,
                        map_value,
                        &next_path,
                        ancestry,
                        &mut Vec::new(),
                        assignment,
                    );
                    let has_coarse_error =
                        child.findings.local.iter().any(|(_, error)| {
                            !matches!(error, TypeError::FieldGroupViolation { .. })
                        });
                    if let Some(error) = child.local.error().cloned().filter(|_| has_coarse_error) {
                        // The collector's coarse diagnostic always uses
                        // MapValueError, even when the primary inner error
                        // is a field group with its own precise path.
                        first_value_error.get_or_insert(TypeError::MapValueError {
                            key: map_key.clone(),
                            inner: Box::new(error),
                        });
                    }
                    let child = child.wrap_local_errors(|error| error.at_map_value(map_key));
                    // Ordinary map entries retain the collector's first-error
                    // precedence. Undecided branches each retain a finding;
                    // field groups already have precise findings below.
                    additional_value_errors.extend(
                        child
                            .findings
                            .additional_local_errors
                            .iter()
                            .filter(|error| !matches!(error, TypeError::FieldGroupViolation { .. }))
                            .cloned(),
                    );
                    if let Some(error) = child.local.error().cloned() {
                        checked.reject(path, error);
                    }
                    checked
                        .findings
                        .assignment
                        .extend(child.findings.assignment);
                    checked.findings.located.extend(child.findings.located);
                    checked
                        .findings
                        .local
                        .extend(child.findings.local.into_iter().filter(|(_, error)| {
                            matches!(error, TypeError::FieldGroupViolation { .. })
                        }));
                    checked.findings.provider.extend(child.findings.provider);
                    checked
                        .findings
                        .additional_local_errors
                        .extend(child.findings.additional_local_errors);
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
                checked.findings.local.extend(
                    additional_value_errors
                        .into_iter()
                        .map(|error| (path.clone(), error)),
                );
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
            _ => match value {
                ClassifiedRef::Known(value) => self.leaf(attr, value, path, ancestry, visited),
                ClassifiedRef::Shape(_) => CheckedValue::rejected(
                    path,
                    TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
                ),
                ClassifiedRef::Pending(_) => {
                    unreachable!("numeric pending values are fitted above")
                }
                ClassifiedRef::Failed(_) => unreachable!("attempted failures were handled above"),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn list<'v, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'v, P, B>>(
        &self,
        attr: &'a CompiledAttributeType,
        inner: &'a CompiledAttributeType,
        constraints: ListConstraints,
        items: &[ClassifiedRef<'_, 'v, P, B, T>],
        string_list: bool,
        path: &FieldPath,
        ancestry: FieldAncestry,
        assignment: AssignmentCheck<'_, P, B>,
    ) -> CheckedValue<'a> {
        let mut children = Vec::new();
        let mut child_fits = Vec::new();
        for (i, value) in items.iter().enumerate() {
            let child = self
                .walk_with_assignment(
                    inner,
                    *value,
                    &path.push_index(i),
                    ancestry,
                    &mut Vec::new(),
                    assignment,
                )
                .wrap_local_errors(|error| error.at_list_index(i));
            let (fit, error) = match child.local {
                LocalOutcome::Accepted(fit) => (Some(fit), None),
                LocalOutcome::Rejected {
                    primary: (_, error),
                } => (None, Some(error)),
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
            items.iter().map(|value| value.diagnostic_value()),
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
                checked.reject(path, error.clone());
                checked.findings.push_local(
                    index.map_or_else(|| path.clone(), |i| path.push_index(i)),
                    error,
                );
            } else if let Some(i) = index
                && let Some(error) = &children[i].0
            {
                checked.reject(path, error.clone());
            }
        }
        for (_, findings) in children {
            checked.findings.assignment.extend(findings.assignment);
            checked.findings.located.extend(findings.located);
            checked.findings.local.extend(findings.local);
            checked
                .findings
                .additional_local_errors
                .extend(findings.additional_local_errors);
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

    fn union_with_assignment<
        'v,
        P: PlaceholderKind,
        B: BindingAddress,
        T: StructuralPending<'v, P, B>,
    >(
        &self,
        members: &'a [CompiledAttributeType],
        value: ClassifiedRef<'_, 'v, P, B, T>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
        assignment: AssignmentCheck<'_, P, B>,
    ) -> Result<AdmittedUnion<'a>, Box<UnionFailure<'a>>> {
        let mut best_accepted: Option<AdmittedUnion<'a>> = None;
        let mut failure: Option<BestUnionMemberFailures> = None;
        let mut provider_failure: Option<UnionProviderFailure<'a>> = None;
        let mut sibling_provider_errors = Vec::new();
        let mut sibling_local_errors = Vec::new();
        let mut sibling_assignment = AssignmentFindings::default();
        for member in members {
            let checked =
                self.walk_with_assignment(member, value, path, ancestry, visited, assignment);
            let accepts = checked.accepts();
            if accepts {
                if let LocalOutcome::Accepted(fit) = checked.local {
                    if fit.exactness.is_maximal() {
                        return Ok(AdmittedUnion {
                            selected: UnionFit { member, fit },
                            assignment: checked.findings.assignment,
                        });
                    }
                    if best_accepted
                        .as_ref()
                        .is_none_or(|current| fit > current.selected.fit)
                    {
                        best_accepted = Some(AdmittedUnion {
                            selected: UnionFit { member, fit },
                            assignment: checked.findings.assignment,
                        });
                    }
                }
                continue;
            }
            let judgement = value
                .concrete()
                .map(|concrete| {
                    union_member_judgement_on_path(member, concrete, self.defs, ancestry, visited)
                })
                .unwrap_or_else(UnionMemberJudgement::no_match);
            let error = match checked.local {
                LocalOutcome::Accepted(fit) => {
                    // This is a local fit only. It supports list uniqueness
                    // when every provider rejects, but never Union acceptance.
                    match &mut provider_failure {
                        Some(best) => {
                            if fit > best.local_fit.fit {
                                best.local_fit = UnionFit { member, fit };
                            }
                            // Provider-only failures use count and declaration
                            // order, independently of structural/local fit.
                            if checked.findings.provider.len() < best.provider.len() {
                                best.provider = checked.findings.provider;
                            }
                        }
                        None => {
                            provider_failure = Some(UnionProviderFailure {
                                local_fit: UnionFit { member, fit },
                                provider: checked.findings.provider,
                            })
                        }
                    }
                    continue;
                }
                LocalOutcome::Rejected {
                    primary: (_, error),
                } => error,
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
                    sibling_local_errors = checked.findings.additional_local_errors;
                    sibling_assignment = checked.findings.assignment;
                }
                Some(current) if judgement.failure_rank == current.failure_rank => {
                    current.tied.push(candidate);
                    // Local diagnostics may describe all tied forms, but the
                    // provider findings belong only to the first tied arm.
                }
                Some(_) => {}
                None => {
                    failure = Some(BestUnionMemberFailures::new(
                        judgement.failure_rank,
                        candidate,
                    ));
                    sibling_provider_errors = checked.findings.provider;
                    sibling_local_errors = checked.findings.additional_local_errors;
                    sibling_assignment = checked.findings.assignment;
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
        let error = match (failure, value.concrete()) {
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
            local: LocalOutcome::Rejected {
                primary: (path.clone(), error),
            },
            findings: CheckedFindings {
                additional_local_errors: sibling_local_errors,
                provider: sibling_provider_errors,
                assignment: sibling_assignment,
                ..Default::default()
            },
        }))
    }

    fn leaf<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attr: &'a CompiledAttributeType,
        value: &KnownValue<'_, P, B>,
        path: &FieldPath,
        ancestry: FieldAncestry,
        visited: &mut Vec<&'a str>,
    ) -> CheckedValue<'a> {
        let proof = value;
        let raw = value.to_value();
        let value = &raw;
        let fit = if matches!(attr.kind, AttrTypeKind::Enum { .. }) {
            match inspect_structure_with_defs_on_path(
                attr,
                ClassifiedRef::<P, B>::Known(proof),
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
        if let Some(lookup) = self.lookup
            && !has_native_validator(attr)
        {
            match self.canonical_leaf(attr, proof, checked.local.fit()) {
                Ok(Some(proof)) => {
                    if let Err(error) = lookup.validate(proof) {
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
        value: &KnownValue<'_, P, B>,
        fit: Option<&Fit<'a>>,
    ) -> Result<Option<LocallyValidatedCustomType<'a>>, CanonicalLeafError<'a>> {
        let raw = value.to_value();
        let value = &raw;
        // Native validation already ran. No dispatch means no value expansion
        // or canonical spelling is needed, including for standalone annotations.
        if has_native_validator(attr) {
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
    fn issue_4276_round4_nested_module_if_wraps_every_branch_error() {
        let leaf = AT::refined_string(None, None, Some((None, Some(5))), None);
        let branch = Value::Deferred(DeferredValue::ModuleIf(Box::new(
            crate::parser::DeferredValueIf::new(
                crate::parser::DeferredIfCondition::new(
                    crate::parser::ParsedValue::resource_ref("gate", "flag", Vec::new()),
                    "child/main.crn".into(),
                    2,
                ),
                Value::Concrete(ConcreteValue::String("aaaaaa".into())),
                Value::Concrete(ConcreteValue::String("bbbbbbb".into())),
            ),
        )));
        for (ty, value, context) in [
            (
                AT::list(leaf.clone()),
                Value::Concrete(ConcreteValue::List(vec![branch.clone()])),
                "List item at index 0",
            ),
            (
                AT::struct_(
                    "Named",
                    vec![PatternStructField::optional("name", leaf.clone()).build()],
                ),
                Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                    "name".into(),
                    branch.clone(),
                )]))),
                "Struct field 'name'",
            ),
            (
                AT::map(leaf.clone()),
                Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                    "name".into(),
                    branch.clone(),
                )]))),
                "Map value for key 'name'",
            ),
            (
                AT::list(AT::map(leaf)),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Map(indexmap::IndexMap::from([("name".into(), branch)])),
                )])),
                "List item at index 0: Map value for key 'name'",
            ),
        ] {
            let findings = validate_value_locally(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
            );
            assert_eq!(
                findings
                    .into_errors()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                vec![
                    format!(
                        "{context}: Invalid value 'aaaaaa': length 6 is outside allowed range [, 5]"
                    ),
                    format!(
                        "{context}: Invalid value 'bbbbbbb': length 7 is outside allowed range [, 5]"
                    ),
                ]
            );
        }
    }

    #[test]
    fn issue_4276_round4_raw_resolver_entry_guards_local_validation() {
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(5))),
            None,
        );
        let declaration = crate::parser::IdentifiedCustomTypeDeclaration::new(&ty).unwrap();
        let calls = std::sync::Mutex::new(Vec::new());
        let resolver = |_: crate::parser::CustomTypeValidationTarget<'_>, value: &Value| {
            calls.lock().unwrap().push(value.clone());
            Ok(())
        };
        let target = crate::parser::CustomTypeValidationTarget::Declaration {
            owner: "test",
            declaration,
        };
        let result = crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("aaaaaa"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        );
        assert!(matches!(
            result,
            Err(crate::parser::CustomTypeValidationError::InvalidDeclarationValue { .. })
        ));
        assert!(calls.lock().unwrap().is_empty());
        crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("ok"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![text("ok")]);
    }

    #[test]
    fn issue_4276_round4_resolver_implements_only_proof_dispatch() {
        struct Resolver(std::sync::Mutex<Vec<String>>);
        impl crate::parser::CustomTypeResolver for Resolver {
            fn validate_validated(
                &self,
                owner: &str,
                proof: LocallyValidatedCustomType<'_>,
            ) -> Result<(), crate::parser::CustomTypeValidationError> {
                assert_eq!(owner, "test");
                assert_eq!(proof.declaration().identity(), &TypeIdentity::bare("Short"));
                self.0
                    .lock()
                    .unwrap()
                    .push(proof.canonical().as_str().to_string());
                Ok(())
            }
        }
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(5))),
            None,
        );
        let target = crate::parser::CustomTypeValidationTarget::Declaration {
            owner: "test",
            declaration: crate::parser::IdentifiedCustomTypeDeclaration::new(&ty).unwrap(),
        };
        let resolver = Resolver(Default::default());
        let error = crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("aaaaaa"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "test rejected `Short`: Invalid Short value 'aaaaaa': length 6 is outside allowed range [, 5]"
        );
        assert!(resolver.0.lock().unwrap().is_empty());
        crate::parser::validate_custom_type_value(
            &resolver,
            target,
            &(text("ok"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*resolver.0.lock().unwrap(), vec!["ok"]);
    }

    #[test]
    fn issue_4276_round2_a_local_union_keeps_selected_provider_findings() {
        let remote = |name| AT::refined_string(Some(TypeIdentity::bare(name)), None, None, None);
        let lookup = |declaration: &AT, _: &Value| {
            let identity = crate::provider::identified_custom_type_identity(declaration).unwrap();
            Err(TypeError::ValidationFailed {
                message: format!("{} rejects bad", identity.kind).into(),
            })
        };
        let cases = [
            (
                AT::union(vec![
                    AT::struct_(
                        "Named",
                        vec![
                            CompiledStructField::optional("name", remote("VpcId")).build(),
                            CompiledStructField::required("size", AT::int()).build(),
                        ],
                    ),
                    AT::bool(),
                ]),
                "name",
                "Required attribute 'size' is missing",
            ),
            (
                AT::union(vec![
                    AT::struct_(
                        "First",
                        vec![
                            CompiledStructField::required("v", remote("VpcId")).build(),
                            CompiledStructField::required("x", AT::int()).build(),
                        ],
                    ),
                    AT::struct_(
                        "Second",
                        vec![
                            CompiledStructField::required("v", remote("SubnetId")).build(),
                            CompiledStructField::required("y", AT::int()).build(),
                        ],
                    ),
                ]),
                "v",
                "Expected one of these forms: ('v' and 'x') or ('v' and 'y')",
            ),
        ];
        for (ty, field, local_message) in cases {
            let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                field.into(),
                text("bad"),
            )])));
            let errors = validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                &lookup,
            )
            .into_errors();
            assert_eq!(
                errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                vec![local_message, "Validation failed: VpcId rejects bad",]
            );
        }
    }

    #[test]
    fn issue_4276_round2_b_provider_only_union_uses_count_then_declaration_order() {
        let remote = |name| AT::refined_string(Some(TypeIdentity::bare(name)), None, None, None);
        let lookup = |declaration: &AT, value: &Value| {
            let identity = crate::provider::identified_custom_type_identity(declaration).unwrap();
            if identity.kind == "SubnetId" && value == &text("subnet-1") {
                return Ok(());
            }
            Err(TypeError::ValidationFailed {
                message: format!(
                    "{} rejects {}",
                    identity.kind,
                    if value == &text("bad") {
                        "bad"
                    } else {
                        "subnet-1"
                    }
                )
                .into(),
            })
        };
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("a".into(), text("bad")),
            ("b".into(), text("subnet-1")),
        ])));
        for tie in [false, true] {
            for reverse in [false, true] {
                let mut arms = vec![
                    AT::map(remote("SubnetId")),
                    AT::struct_(
                        "Named",
                        vec![
                            CompiledStructField::required("a", remote("VpcId")).build(),
                            CompiledStructField::required(
                                "b",
                                if tie { AT::string() } else { remote("VpcId") },
                            )
                            .build(),
                        ],
                    ),
                ];
                if reverse {
                    arms.reverse();
                }
                let errors = validate_value_with_lookup(
                    &AT::union(arms),
                    &(value)
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .map(|proof| proof.into_classification())
                        .unwrap(),
                    &BTreeMap::new(),
                    FieldAncestry::WRITABLE,
                    &lookup,
                )
                .into_errors();
                assert_eq!(
                    errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    vec![if tie && reverse {
                        "Validation failed: VpcId rejects bad"
                    } else {
                        "Validation failed: SubnetId rejects bad"
                    }],
                    "tie={tie}, reverse={reverse}"
                );
            }
        }
    }

    #[test]
    fn issue_4276_round2_e_deferred_union_does_not_expand_unloaded_arms() {
        let unloaded = |name| AT {
            kind: AttrTypeKind::UnloadedSchemaType(
                crate::parser::UnloadedSchemaType::try_from(TypeIdentity::bare(name)).unwrap(),
            ),
        };
        let members = vec![unloaded("VpcId"), unloaded("SubnetId")];
        let ty = AT::union(members.clone());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let lookup = |_: &AT, _: &Value| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let defs = BTreeMap::new();
        for value in [
            Value::root_resource_ref("source", "id", Vec::new()),
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: Vec::new(),
                source_line: 1.into(),
            }),
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamAllExports {
                    binding: crate::resource::RootScope::new()
                        .resolve(crate::resource::RelativeBindingName::new("upstream")),
                },
            )),
        ] {
            let checked = ValueValidator {
                defs: &defs,
                lookup: Some(&lookup),
            }
            .walk(
                &ty,
                (&(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap())
                    .into(),
                &FieldPath::new(),
                FieldAncestry::WRITABLE,
                &mut Vec::new(),
            );
            let deferred = matches!(
                checked.local,
                LocalOutcome::Accepted(Fit {
                    kind: FitKind::Deferred(_),
                    ..
                })
            );
            assert_eq!(checked.into_findings().into_errors(), Vec::new());
            assert!(deferred);
        }
        for member in members {
            let errors = validate_value_with_lookup(
                &member,
                &(text("bad"))
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &defs,
                FieldAncestry::WRITABLE,
                &lookup,
            )
            .into_errors();
            assert_eq!(
                errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
                vec!["provider custom types are not loaded"]
            );
            assert!(
                matches!(&errors[0], TypeError::CustomTypeValidation { error, .. } if matches!(error.as_ref(), crate::parser::CustomTypeValidationError::ProvidersNotLoaded))
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn rejected_value_records_its_finding_before_it_is_merged() {
        let path = FieldPath::new().push_field("destination").push_field("arn");
        let error = TypeError::ReadOnlyAttribute { name: "arn".into() };
        let checked = CheckedValue::rejected(&path, error.clone());

        assert_eq!(checked.findings.local, vec![(path, error)]);
    }

    #[test]
    fn issue_4276_round2_f_rejection_retains_a_primary_finding_without_manual_push() {
        let ty = AT::bool();
        let path = FieldPath::new().push_field("params").push_index(2);
        let checked = CheckedValue::rejected(
            &path,
            TypeError::MissingRequired {
                name: "size".into(),
            },
        );
        assert_eq!(
            checked
                .into_findings()
                .local
                .iter()
                .map(|(path, error)| (path.to_string(), error.to_string()))
                .collect::<Vec<_>>(),
            vec![(
                "params[2]".into(),
                "Required attribute 'size' is missing".into()
            ),]
        );
        let pending_value: Value = Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        ));
        let Classification::Pending(pending) = pending_value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap()
        else {
            panic!("pending fixture")
        };
        let mut checked =
            CheckedValue::accepted(&ty, FitExactness::EXACT, FitKind::Deferred(pending.fit()));
        checked.reject(
            &FieldPath::new(),
            TypeError::MissingRequired {
                name: "size".into(),
            },
        );
        let findings = checked.into_findings();
        assert_eq!(
            findings
                .local
                .iter()
                .map(|(path, error)| (path.to_string(), error.to_string()))
                .collect::<Vec<_>>(),
            vec![(String::new(), "Required attribute 'size' is missing".into()),]
        );
        assert_eq!(
            findings
                .into_errors()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["Required attribute 'size' is missing"]
        );
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
        let errors = validate_value_with_lookup(
            &ty,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &defs,
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
                (&(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap())
                    .into(),
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
                Some(
                    CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                        "^x",
                        crate::schema::PatternMatch::Full,
                    ))
                    .unwrap(),
                ),
                None,
                None,
            ),
        ]);
        let value = text("bad");
        assert!(
            validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
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
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
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
    fn issue_4276_round1_module_if_reports_local_errors_and_guards_provider_dispatch() {
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
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
            &lookup,
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![text("ok")],
            "a locally invalid branch must never reach a provider"
        );
        assert_eq!(
            findings
                .local
                .iter()
                .map(|(path, error)| (path.to_string(), error.to_string()))
                .collect::<Vec<_>>(),
            vec![(
                String::new(),
                "Invalid Short value 'too long': length 8 is outside allowed range [, 3]".into()
            ),]
        );
        assert_eq!(
            findings
                .into_errors()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["Invalid Short value 'too long': length 8 is outside allowed range [, 3]",]
        );
    }

    #[test]
    fn issue_4276_round2_c_module_if_reports_each_branchs_local_and_provider_errors() {
        let ty = AT::refined_string(
            Some(TypeIdentity::bare("VpcId")),
            None,
            Some((None, Some(5))),
            None,
        );
        let lookup = |_: &AT, _: &Value| {
            Err(TypeError::ValidationFailed {
                message: "VpcId rejects bad".into(),
            })
        };
        for both_local in [false, true] {
            let value = Value::Deferred(DeferredValue::ModuleIf(Box::new(
                crate::parser::DeferredValueIf::new(
                    crate::parser::DeferredIfCondition::new(
                        Value::Concrete(ConcreteValue::Bool(true)),
                        "child/main.crn".into(),
                        2,
                    ),
                    Value::Concrete(ConcreteValue::String(
                        if both_local { "bad-long" } else { "bad" }.into(),
                    )),
                    Value::Concrete(ConcreteValue::String("vpc-123".into())),
                ),
            )));
            let findings = validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                &lookup,
            );
            assert_eq!(findings.local.len(), if both_local { 2 } else { 1 });
            assert_eq!(
                findings
                    .into_errors()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                if both_local {
                    vec![
                        "Invalid VpcId value 'bad-long': length 8 is outside allowed range [, 5]",
                        "Invalid VpcId value 'vpc-123': length 7 is outside allowed range [, 5]",
                    ]
                } else {
                    vec![
                        "Invalid VpcId value 'vpc-123': length 7 is outside allowed range [, 5]",
                        "Validation failed: VpcId rejects bad",
                    ]
                }
            );
        }
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
        let findings = validate_value_locally(
            &ty,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
        );
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
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
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
            (&(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap())
                .into(),
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
    fn issue_4276_round2_d_string_list_defaults_stay_outside_authored_validation() {
        let wire = carina_provider_protocol::types::Value::StringList(vec!["bad".into()]);
        let decoded = crate::protocol_convert::protocol_schema_default_to_core(&wire).unwrap();
        assert!(matches!(decoded, Value::Concrete(ConcreteValue::List(_))));
        for default in [
            Value::Concrete(ConcreteValue::StringList(vec!["bad".into()])),
            decoded,
        ] {
            let schema = PatternResourceSchema::new("test.Sink")
                .attribute(
                    PatternAttributeSchema::optional(
                        "params",
                        PatternAttributeType::list(PatternAttributeType::refined_string(
                            Some(TypeIdentity::bare("VpcId")),
                            None,
                            None,
                            None,
                        )),
                    )
                    .with_default(default)
                    .build(),
                )
                .validate()
                .unwrap();
            let calls = AtomicUsize::new(0);
            let lookup = |_: &CompiledAttributeType, _: &Value| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(TypeError::ValidationFailed {
                    message: "VpcId rejects bad".into(),
                })
            };
            assert_eq!(schema.validate_with_origins_and_lookup::<crate::resource::NoPlaceholder, AbsoluteBindingAddress>(&HashMap::new(), &|_| false, &lookup), Ok(()));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "omitted defaults are not injected"
            );
            let values: [(Value, _); 2] = [
                (
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::String("bad".into()),
                    )])),
                    Err(vec![
                        "Resource validation failed: VpcId rejects bad".to_string(),
                    ]),
                ),
                (
                    Value::Concrete(ConcreteValue::StringList(vec!["bad".into()])),
                    Ok(()),
                ),
            ];
            for (value, expected) in values {
                let result = schema
                    .validate_with_origins_and_lookup(
                        &crate::schema::classified_attributes(&HashMap::from([(
                            "params".into(),
                            value,
                        )]))
                        .unwrap(),
                        &|_| false,
                        &lookup,
                    )
                    .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>());
                assert_eq!(result, expected);
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "only ordinary List elements reach provider dispatch"
            );
        }
    }

    #[test]
    fn issue_4276_round5_string_list_keeps_local_guards_and_native_callbacks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let ty = CompiledAttributeType::list(CompiledAttributeType::refined_string_with_validator(
            TypeIdentity::bare("Short"),
            None,
            Some((None, Some(3))),
            validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            None,
        ));
        let lookup = |_: &CompiledAttributeType, _: &Value| {
            panic!("StringList items must not reach provider dispatch")
        };
        for (input, valid) in [("ok", true), ("long", false)] {
            let value: Value = Value::Concrete(ConcreteValue::StringList(vec![input.into()]));
            let findings = validate_value_with_lookup(
                &ty,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                &BTreeMap::new(),
                FieldAncestry::WRITABLE,
                &lookup,
            );
            assert_eq!(findings.local_error.is_none(), valid);
            assert!(findings.provider.is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

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
                &crate::schema::classified_attributes(&HashMap::from([("values".into(), value)]))
                    .unwrap(),
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

#[cfg(test)]
mod value_knowledge_failure_contract {
    use super::*;

    #[test]
    fn attempted_failure_keeps_known_sibling_and_container_findings() {
        let attr = CompiledAttributeType::list(CompiledAttributeType::int());
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("wrong-kind".into())),
            Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::env,
                args: Vec::new(),
                reason: crate::resource::UnavailableReason::from_builtin_error(
                    "already attempted".into(),
                ),
                source_line: 0.into(),
            }),
        ]));
        assert!(matches!(
            crate::resource::knowledge::PlanResolved::try_from(
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            ),
            Err(crate::resource::knowledge::KnowledgeError::Unavailable { .. })
        ));
        let findings = validate_value_locally(
            &attr,
            &(value)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
            &BTreeMap::new(),
            FieldAncestry::WRITABLE,
        );
        assert!(
            findings.local_error().is_some(),
            "the known invalid sibling must still be checked"
        );
    }
}
