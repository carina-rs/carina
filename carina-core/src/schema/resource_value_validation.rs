//! One resource-value walk, with both aggregate errors and source targets.
//!
//! Successful child fits are retained for collection equality. Failure
//! attribution consumes the errors from that same walk, including callback
//! results; it never retries validation to recover a diagnostic.

use super::*;
use crate::parser::ValueBranch;

/// Which parser-owned location should anchor a resource value finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueFindingAnchor {
    Key,
    Value,
    Assignment,
    BindingToken(BindingTokenAnchor),
}

/// The binding authority retains both the token and proof of its scalar
/// extent. A container binding cannot authorize replacing that token with a
/// single enum member, even when a failing child is anchored to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingTokenAnchor {
    location: crate::parser::DeclarationLocation,
    scalar: bool,
}

impl BindingTokenAnchor {
    pub fn location(&self) -> &crate::parser::DeclarationLocation {
        &self.location
    }
    pub fn replaceable_location(&self) -> Option<&crate::parser::DeclarationLocation> {
        self.scalar.then_some(&self.location)
    }
    pub(crate) fn for_authority_error(
        location: crate::parser::DeclarationLocation,
        error: &TypeError,
    ) -> Self {
        Self {
            location,
            scalar: matches!(error, TypeError::BindingShadowsEnumMember(_)),
        }
    }
}

/// A source path whose private anchoring data may contain secret map keys.
/// Display and Debug always use its sensitivity-aware projection.
#[derive(Clone, PartialEq, Eq)]
pub struct ValueFindingPath {
    source: FieldPath,
    sensitive: bool,
    branches: Vec<ValueBranch>,
    map_keys: Vec<usize>,
}

impl ValueFindingPath {
    pub(crate) fn new(source: FieldPath) -> Self {
        Self {
            source,
            sensitive: false,
            branches: Vec::new(),
            map_keys: Vec::new(),
        }
    }
    pub(crate) fn with_branches(mut self, branches: &[ValueBranch]) -> Self {
        self.branches = branches.to_vec();
        self
    }

    fn same_authored_path(&self, other: &Self) -> bool {
        self.branches == other.branches
            && self.map_keys == other.map_keys
            && self.source.steps().len() == other.source.steps().len()
            && self
                .source
                .steps()
                .iter()
                .zip(other.source.steps())
                .enumerate()
                .all(|(index, (left, right))| self.map_keys.contains(&index) || left == right)
    }
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }
    pub fn is_indexed(&self) -> bool {
        self.source
            .steps()
            .iter()
            .any(|step| matches!(step, FieldPathStep::Index(_)))
    }
    pub fn key_location<'a, P, T, D, B>(
        &self,
        resource: &'a crate::parser::AnalysisResourceRef<'_, P, T, D, B>,
        name: &str,
    ) -> &'a crate::parser::DeclarationLocation
    where
        P: PlaceholderKind,
        T: crate::resource::ValueKind,
        B: BindingAddress,
        D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    {
        resource.attribute_path_location(name, &self.source)
    }
    pub fn value_location<'a, P, T, D, B>(
        &self,
        resource: &'a crate::parser::AnalysisResourceRef<'_, P, T, D, B>,
        name: &str,
    ) -> &'a crate::parser::DeclarationLocation
    where
        P: PlaceholderKind,
        T: crate::resource::ValueKind,
        B: BindingAddress,
        D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    {
        resource.attribute_value_location(name, &self.source)
    }
}

impl fmt::Display for ValueFindingPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.sensitive {
            self.source.clone().masked_for_secret().fmt(f)
        } else {
            self.source.fmt(f)
        }
    }
}
impl fmt::Debug for ValueFindingPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ValueFindingPath({self})")
    }
}

/// An authored attribute path, or a failure of the resource as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceValueTarget {
    Attribute {
        authored_attribute: String,
        path: ValueFindingPath,
        anchor: ValueFindingAnchor,
    },
    Resource,
}

/// A core value error together with its source target. The error is retained
/// as typed data so consumers can decorate enum diagnostics without parsing text.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceValueFinding {
    pub target: ResourceValueTarget,
    pub error: TypeError,
}

impl ResourceValueFinding {
    /// Compare authored findings across expanded instances. Substituted
    /// values describe an occurrence, while constraints identify its rule.
    /// Callers still retain every occurrence within an individual walk.
    pub fn same_authored_rule(&self, other: &Self) -> bool {
        let same_target = match (&self.target, &other.target) {
            (
                ResourceValueTarget::Attribute {
                    authored_attribute,
                    path,
                    anchor,
                },
                ResourceValueTarget::Attribute {
                    authored_attribute: rattr,
                    path: rpath,
                    anchor: ranchor,
                },
            ) => authored_attribute == rattr && anchor == ranchor && path.same_authored_path(rpath),
            (ResourceValueTarget::Resource, ResourceValueTarget::Resource) => true,
            _ => false,
        };
        same_target && same_validation_rule(&self.error, &other.error)
    }
}

fn same_validation_rule(left: &TypeError, right: &TypeError) -> bool {
    use TypeError::*;
    match left {
        LengthOutOfRange {
            min,
            max,
            attribute,
            type_name,
            ..
        } => matches!(right,
            LengthOutOfRange { min: rmin, max: rmax, attribute: rattr, type_name: rtype, .. }
                if (min, max, attribute, type_name) == (rmin, rmax, rattr, rtype)),
        PatternMismatch {
            pattern,
            attribute,
            type_name,
            ..
        } => matches!(right,
            PatternMismatch { pattern: rpattern, attribute: rattr, type_name: rtype, .. }
                if (pattern, attribute, type_name) == (rpattern, rattr, rtype)),
        InvalidEnumVariant {
            value_kind,
            attribute,
            type_name,
            expected,
            ..
        } => matches!(right,
            InvalidEnumVariant { value_kind: rkind, attribute: rattr, type_name: rtype, expected: rexpected, .. }
                if (value_kind, attribute, type_name, expected) == (rkind, rattr, rtype, rexpected)),
        StringLiteralExpectedEnum {
            attribute,
            type_name,
            expected,
            ..
        } => matches!(right,
            StringLiteralExpectedEnum { attribute: rattr, type_name: rtype, expected: rexpected, .. }
                if (attribute, type_name, expected) == (rattr, rtype, rexpected)),
        DuplicateListElement {
            index,
            first_index,
            attribute,
            ..
        } => matches!(right,
            DuplicateListElement { index: rindex, first_index: rfirst, attribute: rattr, .. }
                if (index, first_index, attribute) == (rindex, rfirst, rattr)),
        ValidationFailed { .. } => matches!(right, ValidationFailed { .. }),
        ResourceValidationFailed { attribute, .. } => matches!(right,
            ResourceValidationFailed { attribute: rattr, .. } if attribute == rattr),
        CustomTypeValidation { error, attribute } => matches!(right,
            CustomTypeValidation { error: rerror, attribute: rattr }
                if attribute == rattr && same_custom_validation_rule(error, rerror)),
        ListItemError { index, inner } => matches!(right,
            ListItemError { index: rindex, inner: rinner } if index == rindex && same_validation_rule(inner, rinner)),
        MapKeyError { inner, .. } => matches!(right,
            MapKeyError { inner: rinner, .. } if same_validation_rule(inner, rinner)),
        MapValueError { inner, .. } => matches!(right,
            MapValueError { inner: rinner, .. } if same_validation_rule(inner, rinner)),
        StructFieldError { field, inner } => matches!(right,
            StructFieldError { field: rfield, inner: rinner } if field == rfield && same_validation_rule(inner, rinner)),
        UnionAlternativesMismatch { failures } => matches!(right,
            UnionAlternativesMismatch { failures: rfailures }
                if failures.iter().count() == rfailures.iter().count()
                    && failures.iter().zip(rfailures.iter()).all(|(left, right)| same_validation_rule(left, right))),
        TypeMismatch { types } => matches!(right,
            TypeMismatch { types: rtypes } if types.expected() == rtypes.expected()),
        BindingShadowsEnumMember(_)
        | MissingRequired { .. }
        | FieldGroupViolation { .. }
        | ReadOnlyAttribute { .. }
        | UnknownAttribute { .. }
        | UnknownStructField { .. }
        | UnionStructMismatch { .. }
        | BlockSyntaxNotAllowed { .. } => left == right,
    }
}

fn same_custom_validation_rule(
    left: &crate::parser::CustomTypeValidationError,
    right: &crate::parser::CustomTypeValidationError,
) -> bool {
    use crate::parser::CustomTypeValidationError::*;
    match left {
        Rejected {
            owner, identity, ..
        } => matches!(right,
            Rejected { owner: rowner, identity: ridentity, .. } if (owner, identity) == (rowner, ridentity)),
        ProviderRejected {
            owner, identity, ..
        } => matches!(right,
            ProviderRejected { owner: rowner, identity: ridentity, .. } if (owner, identity) == (rowner, ridentity)),
        InvalidDeclarationValue {
            owner,
            identity,
            error,
        } => matches!(right,
            InvalidDeclarationValue { owner: rowner, identity: ridentity, error: rerror }
                if (owner, identity) == (rowner, ridentity) && same_validation_rule(error, rerror)),
        ProvidersNotLoaded
        | MissingDeclaration { .. }
        | ConflictingEnumDefinitions { .. }
        | ConflictingEnumSpelling { .. } => left == right,
    }
}

/// Resource inputs whose already-checked binding sinks can only be selected
/// by the binding authority. Plain value callers start with `new`.
pub struct ResourceValueInputs<'a, P: PlaceholderKind, B: BindingAddress> {
    pub(crate) attributes: &'a HashMap<String, Value<P, B>>,
    pub(crate) pending: HashSet<String>,
    pub(crate) bindings: Vec<ResourceBindingValidation>,
}

impl<'a, P: PlaceholderKind, B: BindingAddress> ResourceValueInputs<'a, P, B> {
    pub fn new(attributes: &'a HashMap<String, Value<P, B>>) -> Self {
        Self {
            attributes,
            pending: HashSet::new(),
            bindings: Vec::new(),
        }
    }
}

#[derive(Default)]
pub(super) struct ResourceValueReport {
    pub findings: Vec<ResourceValueFinding>,
    pub errors: Vec<TypeError>,
}

impl ResourceValueReport {
    pub fn into_result(self) -> Result<(), Vec<TypeError>> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors)
        }
    }

    fn resource_error(&mut self, error: TypeError) {
        self.errors.push(error.clone());
        self.findings.push(ResourceValueFinding {
            target: ResourceValueTarget::Resource,
            error,
        });
    }
}

struct PathFinding {
    path: ValueFindingPath,
    anchor: ValueFindingAnchor,
    error: TypeError,
}

impl PathFinding {
    fn new(path: &FieldPath, error: TypeError) -> Self {
        let anchor = if matches!(error, TypeError::DuplicateListElement { .. })
            || (path.is_empty() && enum_error(&error))
        {
            ValueFindingAnchor::Value
        } else {
            ValueFindingAnchor::Key
        };
        Self {
            path: ValueFindingPath::new(path.clone()),
            anchor,
            error,
        }
    }
}

fn enum_error(error: &TypeError) -> bool {
    match error {
        TypeError::InvalidEnumVariant { .. } | TypeError::StringLiteralExpectedEnum { .. } => true,
        TypeError::UnionAlternativesMismatch { failures } => failures.iter().any(enum_error),
        _ => false,
    }
}

/// `primary` retains the ordinary aggregate error (including container
/// wrappers). `local` retains all distinct leaf findings. Provider lookups
/// have their own aggregate semantics, but run at the very same leaf visit.
struct CheckedValue<'a> {
    fit: Option<Fit<'a>>,
    primary: Option<TypeError>,
    local: Vec<PathFinding>,
    external: Vec<PathFinding>,
}

impl<'a> CheckedValue<'a> {
    fn accepted(
        attr: &'a CompiledAttributeType,
        exactness: FitExactness,
        kind: FitKind<'a>,
    ) -> Self {
        Self {
            fit: Some(Fit::with_kind(exactness, attr, kind)),
            primary: None,
            local: Vec::new(),
            external: Vec::new(),
        }
    }

    fn rejected(path: &FieldPath, error: TypeError) -> Self {
        Self {
            fit: None,
            primary: Some(error.clone()),
            local: vec![PathFinding::new(path, error)],
            external: Vec::new(),
        }
    }

    fn mask(mut self) -> Self {
        self.primary = self.primary.map(TypeError::masked_for_secret);
        for finding in self.local.iter_mut().chain(&mut self.external) {
            finding.error = finding.error.clone().masked_for_secret();
            finding.path.sensitive = true;
        }
        self
    }
}

impl PatternResourceSchema<Compiled> {
    pub(super) fn collect_resource_values<P: PlaceholderKind, B: BindingAddress>(
        &self,
        inputs: ResourceValueInputs<'_, P, B>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> ResourceValueReport {
        self.collect_resource_values_projected(inputs, is_string_literal, lookup, &mut |_, _, _| {})
    }

    pub(super) fn collect_resource_values_projected<
        P: PlaceholderKind,
        B: BindingAddress,
        Project: FnMut(&str, &Value<P, B>, &Fit<'_>),
    >(
        &self,
        inputs: ResourceValueInputs<'_, P, B>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
        project: &mut Project,
    ) -> ResourceValueReport {
        let mut report = ResourceValueReport::default();
        let attributes = inputs.attributes;
        let aliases = self.block_name_map();
        for (name, schema) in &self.attributes {
            if schema.is_required()
                && !attributes.contains_key(name)
                && !attributes
                    .keys()
                    .chain(&inputs.pending)
                    .any(|authored| aliases.get(authored).unwrap_or(authored) == name)
                && schema.default.is_none()
            {
                report.resource_error(TypeError::MissingRequired { name: name.clone() });
            }
        }
        let values = self.collect_value_entries_projected(
            attributes,
            is_string_literal,
            lookup,
            ValueEntryValidation::FullInput,
            None,
            &inputs.bindings,
            project,
        );
        report.errors.extend(values.errors);
        report.findings.extend(values.findings);
        // Authority failures can reserve a pending slot with no runtime value.
        // Emit them independently of the walk, preserving their authored token
        // instead of folding them into a container's aggregate error.
        for binding in &inputs.bindings {
            if let ResourceBindingValueOutcome::Rejected(Some(error)) = &binding.outcome {
                report.errors.push(error.clone());
                report.findings.push(ResourceValueFinding {
                    target: ResourceValueTarget::Attribute {
                        authored_attribute: binding.attribute.clone(),
                        path: ValueFindingPath::new(binding.path.clone())
                            .with_branches(&binding.branches),
                        anchor: ValueFindingAnchor::BindingToken(binding.anchor_for(&binding.path)),
                    },
                    error: error.clone(),
                });
            }
        }
        for group in &self.field_groups {
            let present = group
                .fields()
                .iter()
                .filter(|name| attributes.contains_key(name.as_str()))
                .count();
            if !group.is_satisfied_by_count(present) {
                report.resource_error(TypeError::FieldGroupViolation {
                    path: FieldPath::new(),
                    group: group.clone(),
                });
            }
        }
        if let Some(validator) = self.validator()
            && let Some(attributes) = attributes_without_placeholders(attributes)
            && let Err(errors) = run_resource_validator(validator, &attributes)
        {
            for error in errors {
                // A resource rule can identify an authored top-level input;
                // otherwise it belongs to the resource type token.
                let target = error
                    .attribute_name()
                    .filter(|name| attributes.contains_key(*name))
                    .map_or(ResourceValueTarget::Resource, |name| {
                        ResourceValueTarget::Attribute {
                            authored_attribute: name.to_string(),
                            path: ValueFindingPath::new(FieldPath::new()),
                            anchor: ValueFindingAnchor::Key,
                        }
                    });
                report.errors.push(error.clone());
                report.findings.push(ResourceValueFinding { target, error });
            }
        }
        report
    }

    pub(super) fn collect_value_entries<P: PlaceholderKind, B: BindingAddress>(
        &self,
        attributes: &HashMap<String, Value<P, B>>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
        mode: ValueEntryValidation,
        selected: Option<&HashSet<String>>,
        bindings: &[ResourceBindingValidation],
    ) -> ResourceValueReport {
        self.collect_value_entries_projected(
            attributes,
            is_string_literal,
            lookup,
            mode,
            selected,
            bindings,
            &mut |_, _, _| {},
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_value_entries_projected<
        P: PlaceholderKind,
        B: BindingAddress,
        Project: FnMut(&str, &Value<P, B>, &Fit<'_>),
    >(
        &self,
        attributes: &HashMap<String, Value<P, B>>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
        mode: ValueEntryValidation,
        selected: Option<&HashSet<String>>,
        bindings: &[ResourceBindingValidation],
        project: &mut Project,
    ) -> ResourceValueReport {
        let mut report = ResourceValueReport::default();
        let aliases = self.block_name_map();
        let mut known: Vec<&str> = self.attributes.keys().map(String::as_str).collect();
        known.extend(aliases.keys().map(String::as_str));
        for (name, value) in attributes {
            if selected.is_some_and(|selected| !selected.contains(name)) {
                continue;
            }
            let canonical = aliases.get(name).map(String::as_str).unwrap_or(name);
            let Some(attribute) = self.attributes.get(canonical) else {
                if mode == ValueEntryValidation::FullInput {
                    let id_attribute = format!("{name}_id");
                    let suggestion = if known.contains(&id_attribute.as_str()) {
                        Some(id_attribute)
                    } else {
                        suggest_similar_name(name, &known)
                    };
                    let error = TypeError::UnknownAttribute {
                        name: name.clone(),
                        suggestion,
                    };
                    report.errors.push(error.clone());
                    report.findings.push(ResourceValueFinding {
                        target: ResourceValueTarget::Attribute {
                            authored_attribute: name.clone(),
                            path: ValueFindingPath::new(FieldPath::new()),
                            anchor: ValueFindingAnchor::Assignment,
                        },
                        error,
                    });
                }
                continue;
            };
            let mut walk = ValueWalk::new(Some(lookup));
            walk.bindings = bindings
                .iter()
                .filter(|binding| binding.attribute == *name)
                .cloned()
                .collect();
            walk.previous = walk
                .bindings
                .iter()
                .flat_map(|binding| {
                    binding.checked().into_iter().flat_map(|checked| {
                        checked.leaves.iter().cloned().map(|mut leaf| {
                            leaf.branches.splice(0..0, binding.branches.clone());
                            leaf
                        })
                    })
                })
                .collect();
            // Native callback results do not depend on portable refinements
            // in another union declaration. Carry readiness outcomes across.
            walk.callbacks = walk
                .bindings
                .iter()
                .flat_map(|binding| {
                    binding.checked().into_iter().flat_map(|checked| {
                        checked.callbacks.iter().cloned().map(|mut callback| {
                            callback.branches.splice(0..0, binding.branches.clone());
                            callback
                        })
                    })
                })
                .collect();
            let mut checked = if attribute.is_read_only() && mode == ValueEntryValidation::FullInput
            {
                CheckedValue::rejected(
                    &FieldPath::new(),
                    TypeError::ReadOnlyAttribute { name: name.clone() },
                )
            } else {
                check_value(
                    &attribute.attr_type,
                    value,
                    &self.defs,
                    FieldAncestry::from_input_mode(attribute.input_mode()),
                    &FieldPath::new(),
                    &mut walk,
                    &mut Vec::new(),
                )
            };
            if checked.primary.is_none()
                && checked.external.is_empty()
                && let Some(fit) = &checked.fit
            {
                project(name, value, fit);
            }
            let quoted_enum = is_string_literal(name)
                && matches!(
                    attribute.attr_type.shape_with_defs(&self.defs),
                    Shape::Enum { .. }
                );
            let tag = |error: TypeError| {
                let error = error.with_attribute(name);
                if quoted_enum {
                    error.into_string_literal_diagnostic()
                } else {
                    error
                }
            };
            report.errors.extend(checked.primary.take().map(tag));
            report.errors.extend(
                checked
                    .external
                    .iter()
                    .map(|finding| tag(finding.error.clone())),
            );
            for finding in checked.local.into_iter().chain(checked.external) {
                report.findings.push(ResourceValueFinding {
                    target: ResourceValueTarget::Attribute {
                        authored_attribute: name.clone(),
                        path: finding.path.clone(),
                        anchor: finding.anchor,
                    },
                    error: if quoted_enum {
                        finding.error.into_string_literal_diagnostic()
                    } else {
                        finding.error
                    },
                });
            }
        }
        report
    }
}

/// Results belong to individual scalar leaves, including failed callbacks.
/// Containers are always revisited so counts, groups and writability remain
/// independent of a bound leaf's validation result.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct BindingValueValidation {
    source: Option<Value>,
    leaves: Vec<CheckedBindingLeaf>,
    callbacks: Vec<CheckedCallback>,
}

impl fmt::Debug for BindingValueValidation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BindingValueValidation")
            .field("leaves", &self.leaves)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq)]
enum LeafInput {
    Value(Value),
    MapKey(String),
}

#[derive(Clone)]
struct CheckedBindingLeaf {
    kind: CallbackInputKind,
    path: FieldPath,
    branches: Vec<ValueBranch>,
    declaration: String,
    native: Option<CustomValidator>,
    input: LeafInput,
    result: LeafResult,
}

#[derive(Clone)]
struct CheckedCallback {
    kind: CallbackInputKind,
    path: FieldPath,
    branches: Vec<ValueBranch>,
    callback: CallbackIdentity,
    input: Value,
    result: Result<(), TypeError>,
}

#[derive(Clone)]
enum CallbackIdentity {
    Native(CustomValidator),
    // A walk's lookup is bound to one schema owner. Provider callbacks use
    // that owner's semantic identity; declaration refinements were checked
    // locally and are not part of the callback's input (provider.rs).
    Provider(TypeIdentity),
}

impl PartialEq for CallbackIdentity {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(left), Self::Native(right)) => Arc::ptr_eq(left, right),
            (Self::Provider(left), Self::Provider(right)) => left == right,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallbackInputKind {
    Value,
    MapKey,
}

impl PartialEq for CheckedCallback {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.branches == other.branches
            && self.kind == other.kind
            && self.callback == other.callback
            && self.input == other.input
            && self.result == other.result
    }
}

impl fmt::Debug for CheckedBindingLeaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CheckedBindingLeaf")
    }
}
impl PartialEq for CheckedBindingLeaf {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.path == other.path
            && self.branches == other.branches
            && self.declaration == other.declaration
            && same_callback(self.native.as_ref(), other.native.as_ref())
            && self.input == other.input
            && self.result == other.result
    }
}
fn same_callback(left: Option<&CustomValidator>, right: Option<&CustomValidator>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}
fn native_callback(attr: &CompiledAttributeType) -> Option<&CustomValidator> {
    match &attr.kind {
        AttrTypeKind::String { binding, .. }
        | AttrTypeKind::Int { binding, .. }
        | AttrTypeKind::Float { binding, .. } => binding.callback(),
        AttrTypeKind::Enum { validate, .. } => validate.as_ref(),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
struct LeafResult {
    fit: Option<(FitExactness, LeafFit)>,
    primary: Option<TypeError>,
    // Binding readiness checks portable/native constraints only. Provider
    // lookups belong to resource validation, which also aggregates missing inputs.
    external: Option<Vec<TypeError>>,
}
#[derive(Clone, Debug, PartialEq)]
enum LeafFit {
    Scalar,
    Enum(CanonicalEnumValue),
}

impl BindingValueValidation {
    pub(crate) fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }
}

impl LeafResult {
    fn capture(checked: &CheckedValue<'_>, lookup_checked: bool) -> Option<Self> {
        let fit = match &checked.fit {
            None => None,
            Some(fit) => Some((
                fit.exactness,
                match &fit.kind {
                    FitKind::PassThrough => LeafFit::Scalar,
                    FitKind::Enum { canonical, .. } => LeafFit::Enum(canonical.clone()),
                    _ => return None,
                },
            )),
        };
        Some(Self {
            fit,
            primary: checked.primary.clone(),
            external: lookup_checked.then(|| {
                checked
                    .external
                    .iter()
                    .map(|finding| finding.error.clone())
                    .collect()
            }),
        })
    }
    fn restore<'a>(&self, attr: &'a CompiledAttributeType, path: &FieldPath) -> CheckedValue<'a> {
        CheckedValue {
            fit: self.fit.as_ref().map(|(exactness, kind)| {
                Fit::with_kind(
                    *exactness,
                    attr,
                    match kind {
                        LeafFit::Scalar => FitKind::PassThrough,
                        LeafFit::Enum(canonical) => FitKind::Enum {
                            canonical: canonical.clone(),
                            rewrite: true,
                        },
                    },
                )
            }),
            primary: self.primary.clone(),
            local: self
                .primary
                .iter()
                .cloned()
                .map(|error| PathFinding::new(path, error))
                .collect(),
            external: self
                .external
                .iter()
                .flatten()
                .cloned()
                .map(|error| PathFinding::new(path, error))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResourceBindingValidation {
    pub attribute: String,
    pub path: FieldPath,
    pub location: crate::parser::DeclarationLocation,
    pub branches: Vec<ValueBranch>,
    pub outcome: ResourceBindingValueOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ResourceBindingValueOutcome {
    Checked(BindingValueValidation),
    // The authority owns this leaf's failure, including when parsing omitted
    // its runtime value. Non-schema failures use the opaque binding report.
    Rejected(Option<TypeError>),
}

impl ResourceBindingValidation {
    fn checked(&self) -> Option<&BindingValueValidation> {
        match &self.outcome {
            ResourceBindingValueOutcome::Checked(checked) => Some(checked),
            ResourceBindingValueOutcome::Rejected(_) => None,
        }
    }

    fn anchor_for(&self, path: &FieldPath) -> BindingTokenAnchor {
        fn scalar(value: &Value) -> bool {
            match value {
                Value::Deferred(DeferredValue::Secret(inner)) => scalar(inner),
                Value::Concrete(
                    ConcreteValue::List(_) | ConcreteValue::Map(_) | ConcreteValue::StringList(_),
                ) => false,
                Value::Concrete(_) => true,
                _ => false,
            }
        }
        let scalar = self.path == *path
            && match &self.outcome {
                ResourceBindingValueOutcome::Checked(checked) => {
                    checked.source.as_ref().is_some_and(scalar)
                }
                ResourceBindingValueOutcome::Rejected(error) => {
                    matches!(error, Some(TypeError::BindingShadowsEnumMember(_)))
                }
            };
        BindingTokenAnchor {
            location: self.location.clone(),
            scalar,
        }
    }
}

struct ValueWalk<'a> {
    lookup: Option<CustomTypeLookup<'a>>,
    previous: Vec<CheckedBindingLeaf>,
    recorded: Vec<CheckedBindingLeaf>,
    bindings: Vec<ResourceBindingValidation>,
    branches: Vec<ValueBranch>,
    callbacks: Vec<CheckedCallback>,
    input_kind: CallbackInputKind,
}

impl<'a> ValueWalk<'a> {
    fn new(lookup: Option<CustomTypeLookup<'a>>) -> Self {
        Self {
            lookup,
            previous: Vec::new(),
            recorded: Vec::new(),
            bindings: Vec::new(),
            branches: Vec::new(),
            callbacks: Vec::new(),
            input_kind: CallbackInputKind::Value,
        }
    }

    fn native_validation(
        &mut self,
        callback: &CustomValidator,
        input: &Value,
        path: &FieldPath,
    ) -> Result<(), TypeError> {
        self.callback_validation(
            CallbackIdentity::Native(callback.clone()),
            input,
            path,
            || callback(input),
        )
    }

    fn callback_validation(
        &mut self,
        callback: CallbackIdentity,
        input: &Value,
        path: &FieldPath,
        validate: impl FnOnce() -> Result<(), TypeError>,
    ) -> Result<(), TypeError> {
        let kind = self.input_kind;
        if let Some(checked) = self.callbacks.iter().find(|checked| {
            checked.path == *path
                && checked.branches == self.branches
                && checked.kind == kind
                && checked.callback == callback
                && checked.input == *input
        }) {
            return checked.result.clone();
        }
        let result = validate();
        // Missing declarations and declaration-specific admission errors are
        // not provider callback outcomes. Do not reuse those for another arm.
        let completed_callback = matches!(callback, CallbackIdentity::Native(_))
            || match &result {
                Ok(()) => true,
                Err(TypeError::CustomTypeValidation { error, .. }) => matches!(
                    error.as_ref(),
                    crate::parser::CustomTypeValidationError::Rejected { .. }
                        | crate::parser::CustomTypeValidationError::ProviderRejected { .. }
                ),
                Err(_) => false,
            };
        if completed_callback {
            self.callbacks.push(CheckedCallback {
                kind,
                path: path.clone(),
                branches: self.branches.clone(),
                callback,
                input: input.clone(),
                result: result.clone(),
            });
        }
        result
    }
}

pub(crate) fn check_binding_value<P: PlaceholderKind, B: BindingAddress>(
    attr: &CompiledAttributeType,
    value: &Value<P, B>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    validation: &mut BindingValueValidation,
) -> Result<(), TypeError> {
    if ancestry.provider_populated {
        return Ok(());
    }
    validation.source = value
        .to_expanded()
        .and_then(Value::try_into_address_free_absolute);
    let mut walk = ValueWalk::new(None);
    let checked = check_value(
        attr,
        value,
        defs,
        ancestry,
        path,
        &mut walk,
        &mut Vec::new(),
    );
    validation.leaves = walk.recorded;
    validation.callbacks = walk.callbacks;
    checked.primary.map_or(Ok(()), Err)
}

fn check_value<'a, P: PlaceholderKind, B: BindingAddress>(
    attr: &'a CompiledAttributeType,
    value: &Value<P, B>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    walk: &mut ValueWalk<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> CheckedValue<'a> {
    if walk.bindings.iter().any(|binding| {
        binding.path == *path
            && binding.branches == walk.branches
            && matches!(binding.outcome, ResourceBindingValueOutcome::Rejected(_))
    }) {
        return CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Deferred);
    }
    // The editor can hand off an authored binding token rather than its
    // substituted value. Keep the binding authority's source at this seam,
    // including containers whose independent constraints must still be walked.
    let binding = walk
        .bindings
        .iter()
        .find(|binding| binding.path == *path && binding.branches == walk.branches)
        .cloned();
    let source = binding
        .as_ref()
        .and_then(ResourceBindingValidation::checked)
        .and_then(|checked| checked.source.clone());
    let mut checked = match source {
        Some(source) => check_input(
            attr,
            CheckInput::Value(&source),
            defs,
            ancestry,
            path,
            walk,
            visited_refs,
        ),
        None => check_input(
            attr,
            CheckInput::Value(value),
            defs,
            ancestry,
            path,
            walk,
            visited_refs,
        ),
    };
    if let Some(binding) = binding {
        for finding in checked.local.iter_mut().chain(&mut checked.external) {
            finding.anchor =
                ValueFindingAnchor::BindingToken(binding.anchor_for(&finding.path.source));
        }
    }
    checked
}

enum CheckInput<'a, P: PlaceholderKind, B: BindingAddress> {
    Value(&'a Value<P, B>),
    MapKey(&'a str),
}

fn check_input<'a, P: PlaceholderKind, B: BindingAddress>(
    attr: &'a CompiledAttributeType,
    value: CheckInput<'_, P, B>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    walk: &mut ValueWalk<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> CheckedValue<'a> {
    let previous_kind = walk.input_kind;
    if matches!(&value, CheckInput::MapKey(_)) {
        walk.input_kind = CallbackInputKind::MapKey;
    }
    let mut checked = check_input_inner(attr, value, defs, ancestry, path, walk, visited_refs);
    for finding in checked.local.iter_mut().chain(&mut checked.external) {
        if finding.path.branches.is_empty() {
            finding.path.branches = walk.branches.clone();
        }
    }
    walk.input_kind = previous_kind;
    checked
}

fn check_input_inner<'a, P: PlaceholderKind, B: BindingAddress>(
    attr: &'a CompiledAttributeType,
    value: CheckInput<'_, P, B>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    walk: &mut ValueWalk<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> CheckedValue<'a> {
    if let CheckInput::Value(Value::Deferred(DeferredValue::Secret(inner))) = value {
        let mut checked = check_input(
            attr,
            CheckInput::Value(inner),
            defs,
            ancestry.inside_secret(),
            path,
            walk,
            visited_refs,
        )
        .mask();
        checked.fit = checked
            .fit
            .map(|fit| Fit::with_kind(fit.exactness, attr, FitKind::Secret(Box::new(fit))));
        return checked;
    }
    let resolution = match resolve_refs_on_path(attr, defs, visited_refs) {
        Ok(resolution) => resolution,
        Err(error) => return CheckedValue::rejected(path, error.into_validation_error()),
    };
    let input = match &value {
        CheckInput::Value(value) => {
            let scalar = !matches!(
                resolution.attr.kind,
                AttrTypeKind::List { .. }
                    | AttrTypeKind::Map { .. }
                    | AttrTypeKind::Struct { .. }
                    | AttrTypeKind::Union(_)
                    | AttrTypeKind::Ref(_)
            ) && value.as_concrete().is_some();
            scalar
                .then(|| {
                    value
                        .to_expanded()
                        .and_then(Value::try_into_address_free_absolute)
                })
                .flatten()
                .map(LeafInput::Value)
        }
        // Key lifting may invoke a native enum validator. Cache the authored
        // key before lifting so both accepted and rejected resolutions reuse it.
        CheckInput::MapKey(key) => Some(LeafInput::MapKey((*key).into())),
    };
    let declaration = input
        .as_ref()
        .map(|_| resolution.attr.declaration_fingerprint());
    let cached = walk
        .recorded
        .iter()
        .chain(&walk.previous)
        .find(|leaf| {
            leaf.kind == walk.input_kind
                && leaf.path == *path
                && leaf.branches == walk.branches
                && Some(&leaf.declaration) == declaration.as_ref()
                && Some(&leaf.input) == input.as_ref()
                && same_callback(leaf.native.as_ref(), native_callback(resolution.attr))
        })
        .cloned();
    if let Some(mut cached) = cached {
        let mut checked = cached.result.restore(resolution.attr, path);
        if cached.result.external.is_none() && walk.lookup.is_some() {
            apply_leaf_lookup(&mut checked, resolution.attr, &cached.input, path, walk);
            cached.result = LeafResult::capture(&checked, true).expect("cached scalar leaf");
            walk.recorded.push(cached);
        }
        visited_refs.truncate(resolution.restore_len);
        return checked;
    }
    let mut checked = match value {
        CheckInput::Value(value) => check_resolved_value(
            resolution.attr,
            value,
            defs,
            ancestry,
            path,
            walk,
            visited_refs,
        ),
        CheckInput::MapKey(key) => {
            match lift_map_key_with::<P, B>(resolution.attr, key, defs, &mut |callback, input| {
                walk.native_validation(callback, input, path)
            }) {
                Ok(lifted) => check_resolved_value(
                    resolution.attr,
                    &lifted,
                    defs,
                    ancestry,
                    path,
                    walk,
                    visited_refs,
                ),
                Err(error) => CheckedValue::rejected(path, error),
            }
        }
    };
    if let Some(input) = &input {
        apply_leaf_lookup(&mut checked, resolution.attr, input, path, walk);
    }
    if let (Some(input), Some(declaration), Some(result)) = (
        input,
        declaration,
        LeafResult::capture(&checked, walk.lookup.is_some()),
    ) {
        walk.recorded.push(CheckedBindingLeaf {
            kind: walk.input_kind,
            path: path.clone(),
            branches: walk.branches.clone(),
            declaration,
            native: native_callback(resolution.attr).cloned(),
            input,
            result,
        });
    }
    visited_refs.truncate(resolution.restore_len);
    checked
}

fn apply_leaf_lookup(
    checked: &mut CheckedValue<'_>,
    attr: &CompiledAttributeType,
    input: &LeafInput,
    path: &FieldPath,
    walk: &mut ValueWalk<'_>,
) {
    // A provider declaration bridge may check portable constraints too. Only
    // a successful local fit is eligible; rejected leaves keep their result.
    if checked.fit.is_none() {
        return;
    }
    let Some(lookup) = walk.lookup else {
        return;
    };
    let (identity, value) = match &attr.kind {
        AttrTypeKind::Enum {
            identity,
            validate: None,
            ..
        } => match &checked.fit {
            Some(Fit {
                kind: FitKind::Enum { canonical, .. },
                ..
            }) => (
                identity,
                Value::Concrete(ConcreteValue::CanonicalEnum(canonical.clone())),
            ),
            _ => return,
        },
        AttrTypeKind::String { binding, .. }
        | AttrTypeKind::Int { binding, .. }
        | AttrTypeKind::Float { binding, .. }
            if binding.identity().is_some() && binding.callback().is_none() =>
        {
            (
                binding.identity().expect("identified lookup"),
                match input {
                    LeafInput::Value(value) => value.clone(),
                    LeafInput::MapKey(key) => {
                        Value::Concrete(ConcreteValue::String(key.clone().into()))
                    }
                },
            )
        }
        _ => return,
    };
    if let Err(error) = walk.callback_validation(
        CallbackIdentity::Provider(identity.clone()),
        &value,
        path,
        || lookup(attr, &value),
    ) {
        checked.external.push(PathFinding::new(path, error));
    }
}

fn check_resolved_value<'a, P: PlaceholderKind, B: BindingAddress>(
    attr: &'a CompiledAttributeType,
    value: &Value<P, B>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    walk: &mut ValueWalk<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> CheckedValue<'a> {
    if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
        walk.branches.push(ValueBranch::CoalesceValue);
        let mut left = check_value(attr, value, defs, ancestry, path, walk, &mut Vec::new());
        walk.branches.pop();
        walk.branches.push(ValueBranch::CoalesceDefault);
        let right = check_value(attr, default, defs, ancestry, path, walk, &mut Vec::new());
        walk.branches.pop();
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
        left.primary = left.primary.or(right.primary);
        left.local.extend(right.local);
        left.external.extend(right.external);
        return left;
    }
    if let Value::Deferred(DeferredValue::ModuleIf(deferred)) = value {
        let mut checked = CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Deferred);
        let mut fits = Vec::new();
        for (branch, identity) in deferred.values()[1..]
            .iter()
            .zip([ValueBranch::Then, ValueBranch::Else])
        {
            walk.branches.push(identity);
            let branch = check_value(attr, branch, defs, ancestry, path, walk, &mut Vec::new());
            walk.branches.pop();
            if let Some(fit) = branch.fit {
                fits.push(fit);
            }
            checked.primary = checked.primary.or(branch.primary);
            checked.local.extend(branch.local);
            checked.external.extend(branch.external);
        }
        checked.fit = if fits.len() == 2 {
            let else_value = fits.pop().unwrap();
            let then_value = fits.pop().unwrap();
            let mut exactness = then_value.exactness;
            exactness.combine(else_value.exactness);
            Some(Fit::with_kind(
                exactness,
                attr,
                FitKind::Conditional {
                    then_value: Box::new(then_value),
                    else_value: Box::new(else_value),
                },
            ))
        } else {
            None
        };
        return checked;
    }
    let Some(concrete) = value.as_concrete() else {
        // Deferred reference/access/arithmetic compatibility is checked by
        // the expression authority, not by concrete value validation.
        return CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::Deferred);
    };
    match (&attr.kind, concrete) {
        (
            AttrTypeKind::List {
                element_type,
                constraints,
                ..
            },
            ConcreteValueRef::List(items),
        ) => check_list(
            attr,
            element_type,
            *constraints,
            items,
            false,
            defs,
            ancestry,
            path,
            walk,
        ),
        (
            AttrTypeKind::List {
                element_type,
                constraints,
                ..
            },
            ConcreteValueRef::StringList(items),
        ) => {
            let items: Vec<Value<P, B>> = items
                .iter()
                .map(|s| Value::Concrete(ConcreteValue::String(s.clone())))
                .collect();
            check_list(
                attr,
                element_type,
                *constraints,
                &items,
                true,
                defs,
                ancestry,
                path,
                walk,
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
                    checked.primary.get_or_insert_with(|| error.clone());
                    checked.local.push(PathFinding::new(
                        &path.push_field(field.name.clone()),
                        error,
                    ));
                }
            }
            let groups: Vec<_> = violated_struct_field_groups(fields, field_groups, map, ancestry)
                .map(|group| TypeError::FieldGroupViolation {
                    path: path.clone(),
                    group: group.clone(),
                })
                .collect();
            checked.local.extend(
                groups
                    .iter()
                    .cloned()
                    .map(|error| PathFinding::new(path, error)),
            );
            let mut fits = Vec::new();
            let mut exactness = FitExactness::EXACT;
            let known: Vec<_> = fields.iter().map(|field| field.name.as_str()).collect();
            for (key, value) in map {
                let next_path = path.push_field(key.clone());
                let Some(field) = struct_field_for_key(fields, key, ancestry) else {
                    let error = TypeError::UnknownStructField {
                        struct_name: name.clone(),
                        field: key.clone(),
                        suggestion: suggest_similar_name(key, &known),
                    };
                    checked.primary.get_or_insert_with(|| error.clone());
                    checked.local.push(PathFinding::new(&next_path, error));
                    continue;
                };
                if let Some(error) = validate_struct_field_assignment(field, key, ancestry) {
                    checked
                        .primary
                        .get_or_insert_with(|| error.clone().at_struct_field(key));
                    checked.local.push(PathFinding::new(&next_path, error));
                    continue;
                }
                let child = check_value(
                    &field.field_type,
                    value,
                    defs,
                    ancestry.descend(field),
                    &next_path,
                    walk,
                    &mut Vec::new(),
                );
                if let Some(error) = child.primary {
                    checked
                        .primary
                        .get_or_insert_with(|| error.at_struct_field(key));
                }
                if let Some(fit) = child.fit {
                    exactness.combine(fit.exactness);
                    fits.push(fit);
                }
                checked.local.extend(child.local);
                checked.external.extend(child.external);
            }
            if let Some(TypeError::FieldGroupViolation { group, .. }) = groups.into_iter().next() {
                checked
                    .primary
                    .get_or_insert(TypeError::FieldGroupViolation {
                        path: FieldPath::new(),
                        group,
                    });
            }
            checked.fit = checked
                .primary
                .is_none()
                .then(|| Fit::with_kind(exactness, attr, FitKind::Struct(fits)));
            checked
        }
        (
            AttrTypeKind::Map {
                key,
                value: value_type,
            },
            ConcreteValueRef::Map(map),
        ) => {
            let mut checked =
                CheckedValue::accepted(attr, FitExactness::GENERIC_MAP, FitKind::Map(Vec::new()));
            let mut fits = Vec::new();
            let mut exactness = FitExactness::GENERIC_MAP;
            let mut first_value_error = None;
            for (map_key, map_value) in map {
                let next_path = path.push_field(map_key.clone());
                let mut key_checked = check_input::<P, B>(
                    key,
                    CheckInput::MapKey(map_key),
                    defs,
                    ancestry,
                    &next_path,
                    walk,
                    &mut Vec::new(),
                );
                if let Some(error) = key_checked.primary {
                    let error = TypeError::MapKeyError {
                        key: map_key.clone(),
                        inner: Box::new(error),
                    };
                    checked.primary.get_or_insert_with(|| error.clone());
                    let mut finding = PathFinding::new(&next_path, error);
                    finding.path.map_keys.push(path.steps().len());
                    checked.local.push(finding);
                }
                for finding in &mut key_checked.external {
                    finding.path.map_keys.insert(0, path.steps().len());
                }
                checked.external.extend(key_checked.external);
                let mut child = check_value(
                    value_type,
                    map_value,
                    defs,
                    ancestry,
                    &next_path,
                    walk,
                    &mut Vec::new(),
                );
                for finding in child.local.iter_mut().chain(&mut child.external) {
                    finding.path.map_keys.insert(0, path.steps().len());
                }
                if let Some(error) = &child.primary {
                    checked
                        .primary
                        .get_or_insert_with(|| error.clone().at_map_value(map_key));
                }
                // Preserve the established coarse ordinary Map-value target;
                // field groups retain their concrete entry path.
                let has_ordinary_error = child
                    .local
                    .iter()
                    .any(|finding| !matches!(finding.error, TypeError::FieldGroupViolation { .. }));
                if has_ordinary_error && first_value_error.is_none() {
                    first_value_error = child
                        .primary
                        .clone()
                        .map(|error| error.at_map_value(map_key));
                }
                checked
                    .local
                    .extend(child.local.into_iter().filter(|finding| {
                        matches!(finding.error, TypeError::FieldGroupViolation { .. })
                    }));
                checked.external.extend(child.external);
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
                checked.local.push(PathFinding::new(path, error));
            }
            checked.fit = checked
                .primary
                .is_none()
                .then(|| Fit::with_kind(exactness, attr, FitKind::Map(fits)));
            checked
        }
        (AttrTypeKind::Union(members), _) => {
            let resolved_members = UnionMembers::new_after_union_on_path(
                members.as_raw_slice(),
                defs,
                visited_refs.clone(),
            );
            let mut best_fit: Option<CheckedValue<'a>> = None;
            let mut best_failure: Option<BestUnionMemberFailures> = None;
            for member in members.as_raw_slice() {
                let mut checked =
                    check_value(member, value, defs, ancestry, path, walk, visited_refs);
                if let Some(fit) = checked.fit.take() {
                    if best_fit.as_ref().is_none_or(|best| {
                        checked.external.len() < best.external.len()
                            || (checked.external.len() == best.external.len()
                                && Some(&fit) > best.fit.as_ref())
                    }) {
                        checked.fit = Some(Fit::with_kind(
                            fit.exactness,
                            attr,
                            FitKind::Union {
                                member,
                                fit: Box::new(fit),
                            },
                        ));
                        best_fit = Some(checked);
                    }
                    if best_fit.as_ref().is_some_and(|best| {
                        best.external.is_empty()
                            && best
                                .fit
                                .as_ref()
                                .is_some_and(|fit| fit.exactness.is_maximal())
                    }) {
                        break;
                    }
                } else if let Some(error) = checked.primary {
                    let judgement = union_member_judgement_on_path(
                        member,
                        concrete,
                        defs,
                        ancestry,
                        visited_refs,
                    );
                    if !judgement.failure_rank.is_match() {
                        continue;
                    }
                    let failure = UnionMemberFailure {
                        error,
                        diagnostic_identity: judgement.diagnostic_identity,
                    };
                    match &mut best_failure {
                        Some(best) if judgement.failure_rank > best.failure_rank => {
                            *best = BestUnionMemberFailures::new(judgement.failure_rank, failure)
                        }
                        Some(best) if judgement.failure_rank == best.failure_rank => {
                            best.tied.push(failure)
                        }
                        Some(_) => {}
                        None => {
                            best_failure = Some(BestUnionMemberFailures::new(
                                judgement.failure_rank,
                                failure,
                            ))
                        }
                    }
                }
            }
            if let Some(checked) = best_fit {
                checked
            } else {
                let error = best_failure
                    .map(|failure| failure.into_error(concrete, resolved_members, ancestry))
                    .unwrap_or_else(|| {
                        TypeError::value_type_mismatch(attr.type_name(), value.type_name())
                    });
                CheckedValue::rejected(path, error)
            }
        }
        (AttrTypeKind::Enum { identity, .. }, _) => {
            let resolver = EnumValueResolver::with_defs(attr, defs);
            let canonical = match concrete {
                ConcreteValueRef::CanonicalEnum(canonical)
                    if canonical.identity().same_type(identity) =>
                {
                    Ok(canonical.clone())
                }
                ConcreteValueRef::EnumIdentifier(raw) => resolver
                    .resolve_raw_with(raw, &mut |callback, input| {
                        walk.native_validation(callback, input, path)
                    }),
                ConcreteValueRef::String(text) => resolver
                    .resolve_authored_string(text, &mut |callback, input| {
                        walk.native_validation(callback, input, path)
                    }),
                _ => Err(TypeError::value_type_mismatch(
                    attr.type_name(),
                    value.type_name(),
                )),
            };
            match canonical {
                Ok(canonical) => CheckedValue::accepted(
                    attr,
                    if matches!(concrete, ConcreteValueRef::String(_)) {
                        FitExactness::COERCIBLE
                    } else {
                        FitExactness::EXACT
                    },
                    FitKind::Enum {
                        canonical: canonical.clone(),
                        rewrite: true,
                    },
                ),
                Err(error) => CheckedValue::rejected(path, error),
            }
        }
        (AttrTypeKind::Struct { name, .. }, ConcreteValueRef::List(_)) => CheckedValue::rejected(
            path,
            TypeError::BlockSyntaxNotAllowed {
                attribute: name.clone(),
            },
        ),
        (AttrTypeKind::List { .. } | AttrTypeKind::Map { .. } | AttrTypeKind::Struct { .. }, _) => {
            CheckedValue::rejected(
                path,
                TypeError::value_type_mismatch(attr.type_name(), value.type_name()),
            )
        }
        _ => match attr.inspect_primitive_with(concrete, |callback, input| {
            walk.native_validation(callback, input, path)
        }) {
            Ok(()) => CheckedValue::accepted(
                attr,
                if matches!(
                    (&attr.kind, concrete),
                    (AttrTypeKind::Float { .. }, ConcreteValueRef::Int(_))
                ) {
                    FitExactness::COERCIBLE
                } else {
                    FitExactness::EXACT
                },
                FitKind::PassThrough,
            ),
            Err(error) => CheckedValue::rejected(path, error),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn check_list<'a, P: PlaceholderKind, B: BindingAddress>(
    attr: &'a CompiledAttributeType,
    element_type: &'a CompiledAttributeType,
    constraints: ListConstraints,
    items: &[Value<P, B>],
    string_list: bool,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    walk: &mut ValueWalk<'_>,
) -> CheckedValue<'a> {
    let mut children: Vec<_> = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            check_value(
                element_type,
                item,
                defs,
                ancestry,
                &path.push_index(index),
                walk,
                &mut Vec::new(),
            )
        })
        .collect();
    let mut child_fits = children
        .iter_mut()
        .map(|child| child.fit.take())
        .collect::<Vec<_>>()
        .into_iter();
    let mut checked = CheckedValue::accepted(attr, FitExactness::EXACT, FitKind::List(Vec::new()));
    let mut fits = Vec::with_capacity(items.len());
    let mut exactness = FitExactness::EXACT;
    for result in constraints.check(
        items.iter(),
        defs,
        ancestry,
        CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
        |_| child_fits.next().flatten(),
    ) {
        match result {
            Err(ListConstraintError { index, error }) => {
                checked.primary.get_or_insert_with(|| *error.clone());
                checked.local.push(PathFinding::new(
                    &index.map_or_else(|| path.clone(), |index| path.push_index(index)),
                    *error,
                ));
            }
            Ok((index, fit)) => {
                if let Some(fit) = fit {
                    exactness.combine(fit.exactness);
                    fits.push(fit);
                } else if let Some(error) = &children[index].primary {
                    checked
                        .primary
                        .get_or_insert_with(|| error.clone().at_list_index(index));
                }
            }
        }
    }
    for child in children {
        checked.local.extend(child.local);
        checked.external.extend(child.external);
    }
    checked.fit = checked.primary.is_none().then(|| {
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

/// A projection made from the successful fit of the value-validation walk.
/// Canonicalization failures keep their later, existing CLI error boundary.
pub(crate) struct ResourceValueProjection<P: PlaceholderKind, B: BindingAddress> {
    pub canonical: Result<HashMap<String, Value<P, B>>, (String, TypeError)>,
}
