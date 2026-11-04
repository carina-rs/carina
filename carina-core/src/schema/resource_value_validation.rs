//! One resource-value walk, with both aggregate errors and source targets.
//!
//! Successful child fits are retained for collection equality. Failure
//! attribution consumes the errors from that same walk, including callback
//! results; it never retries validation to recover a diagnostic.

use super::*;

/// Which parser-owned location should anchor a resource value finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueFindingAnchor {
    Key,
    Value,
    Assignment,
    BindingToken(crate::parser::DeclarationLocation),
}

/// An authored attribute path, or a failure of the resource as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceValueTarget {
    Attribute {
        authored_attribute: String,
        path: FieldPath,
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

/// Resource inputs whose already-checked binding sinks can only be selected
/// by the binding authority. Plain value callers start with `new`.
pub struct ResourceValueInputs<'a, P: PlaceholderKind, B: BindingAddress> {
    pub(crate) attributes: &'a HashMap<String, Value<P, B>>,
    pub(crate) checked: HashSet<String>,
    pub(crate) binding_findings: Vec<ResourceValueFinding>,
}

impl<'a, P: PlaceholderKind, B: BindingAddress> ResourceValueInputs<'a, P, B> {
    pub fn new(attributes: &'a HashMap<String, Value<P, B>>) -> Self {
        Self {
            attributes,
            checked: HashSet::new(),
            binding_findings: Vec::new(),
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
    path: FieldPath,
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
            path: path.clone(),
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
        let mut report = ResourceValueReport::default();
        let attributes = inputs.attributes;
        for (name, schema) in &self.attributes {
            if schema.is_required()
                && !attributes.contains_key(name)
                && !inputs.checked.contains(name)
                && schema.default.is_none()
            {
                report.resource_error(TypeError::MissingRequired { name: name.clone() });
            }
        }
        let values = self.collect_value_entries(
            attributes,
            is_string_literal,
            lookup,
            ValueEntryValidation::FullInput,
            None,
            &inputs.checked,
        );
        report.errors.extend(values.errors);
        report.findings.extend(values.findings);
        report.findings.extend(inputs.binding_findings);
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
                            path: FieldPath::new(),
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
        checked: &HashSet<String>,
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
                            path: FieldPath::new(),
                            anchor: ValueFindingAnchor::Assignment,
                        },
                        error,
                    });
                }
                continue;
            };
            if checked.contains(name) {
                continue;
            }
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
                    lookup,
                    &mut Vec::new(),
                )
            };
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
                        path: finding.path,
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

fn check_value<'a, P: PlaceholderKind, B: BindingAddress>(
    attr: &'a CompiledAttributeType,
    value: &Value<P, B>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    lookup: CustomTypeLookup<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> CheckedValue<'a> {
    let resolution = match resolve_refs_on_path(attr, defs, visited_refs) {
        Ok(resolution) => resolution,
        Err(error) => return CheckedValue::rejected(path, error.into_validation_error()),
    };
    let checked = check_resolved_value(
        resolution.attr,
        value,
        defs,
        ancestry,
        path,
        lookup,
        visited_refs,
    );
    visited_refs.truncate(resolution.restore_len);
    checked
}

fn check_resolved_value<'a, P: PlaceholderKind, B: BindingAddress>(
    attr: &'a CompiledAttributeType,
    value: &Value<P, B>,
    defs: &'a BTreeMap<String, CompiledAttributeType>,
    ancestry: FieldAncestry,
    path: &FieldPath,
    lookup: CustomTypeLookup<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> CheckedValue<'a> {
    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        let mut checked = check_resolved_value(
            attr,
            inner,
            defs,
            ancestry.inside_secret(),
            path,
            lookup,
            visited_refs,
        )
        .mask();
        checked.fit = checked
            .fit
            .map(|fit| Fit::with_kind(fit.exactness, attr, FitKind::Secret(Box::new(fit))));
        return checked;
    }
    if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
        let mut left = check_value(attr, value, defs, ancestry, path, lookup, &mut Vec::new());
        let right = check_value(attr, default, defs, ancestry, path, lookup, &mut Vec::new());
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
        for branch in &deferred.values()[1..] {
            let branch = check_value(attr, branch, defs, ancestry, path, lookup, &mut Vec::new());
            checked.primary = checked.primary.or(branch.primary);
            checked.local.extend(branch.local);
            checked.external.extend(branch.external);
        }
        if checked.primary.is_some() {
            checked.fit = None;
        }
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
            lookup,
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
                lookup,
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
                    lookup,
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
                let lifted: Value<P, B> = lift_map_key(key, map_key, defs);
                let key_checked = check_value(
                    key,
                    &lifted,
                    defs,
                    ancestry,
                    &next_path,
                    lookup,
                    &mut Vec::new(),
                );
                if let Some(error) = key_checked.primary {
                    let error = TypeError::MapKeyError {
                        key: map_key.clone(),
                        inner: Box::new(error),
                    };
                    checked.primary.get_or_insert_with(|| error.clone());
                    checked.local.push(PathFinding::new(&next_path, error));
                }
                checked.external.extend(key_checked.external);
                let child = check_value(
                    value_type,
                    map_value,
                    defs,
                    ancestry,
                    &next_path,
                    lookup,
                    &mut Vec::new(),
                );
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
            let mut best_fit: Option<Fit<'a>> = None;
            let mut best_failure: Option<BestUnionMemberFailures> = None;
            let mut best_external: Option<Vec<PathFinding>> = None;
            for member in members.as_raw_slice() {
                let checked =
                    check_value(member, value, defs, ancestry, path, lookup, visited_refs);
                if best_external
                    .as_ref()
                    .is_none_or(|best| checked.external.len() < best.len())
                {
                    best_external = Some(checked.external);
                }
                if let Some(fit) = checked.fit {
                    if best_fit.as_ref().is_none_or(|best| fit > *best) {
                        best_fit = Some(Fit::with_kind(
                            fit.exactness,
                            attr,
                            FitKind::Union {
                                member,
                                fit: Box::new(fit),
                            },
                        ));
                    }
                    if best_fit
                        .as_ref()
                        .is_some_and(|fit| fit.exactness.is_maximal())
                        && best_external.as_ref().is_some_and(Vec::is_empty)
                    {
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
            let mut checked = if let Some(fit) = best_fit {
                CheckedValue {
                    fit: Some(fit),
                    primary: None,
                    local: Vec::new(),
                    external: Vec::new(),
                }
            } else {
                let error = best_failure
                    .map(|failure| failure.into_error(concrete, resolved_members, ancestry))
                    .unwrap_or_else(|| {
                        TypeError::value_type_mismatch(attr.type_name(), value.type_name())
                    });
                CheckedValue::rejected(path, error)
            };
            checked.external = best_external.unwrap_or_default();
            checked
        }
        (
            AttrTypeKind::Enum {
                identity, validate, ..
            },
            _,
        ) => {
            let resolver = EnumValueResolver::with_defs(attr, defs);
            let canonical = match concrete {
                ConcreteValueRef::CanonicalEnum(canonical)
                    if canonical.identity().same_type(identity) =>
                {
                    Ok(canonical.clone())
                }
                ConcreteValueRef::EnumIdentifier(raw) => resolver.resolve_raw(raw),
                ConcreteValueRef::String(text) => resolver.resolve_authored_string(text),
                _ => Err(TypeError::value_type_mismatch(
                    attr.type_name(),
                    value.type_name(),
                )),
            };
            match canonical {
                Ok(canonical) => {
                    let mut checked = CheckedValue::accepted(
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
                    );
                    if validate.is_none() {
                        let value = Value::Concrete(ConcreteValue::CanonicalEnum(canonical));
                        if let Err(error) = lookup(attr, &value) {
                            checked.external.push(PathFinding::new(path, error));
                        }
                    }
                    checked
                }
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
        _ => {
            let mut checked = match attr.validate_leaf_value(value) {
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
            };
            let binding = match &attr.kind {
                AttrTypeKind::String { binding, .. }
                | AttrTypeKind::Int { binding, .. }
                | AttrTypeKind::Float { binding, .. } => Some(binding),
                _ => None,
            };
            if let Some(binding) = binding
                && binding.identity().is_some()
                && binding.callback().is_none()
                && let Some(value) = value
                    .to_expanded()
                    .and_then(Value::try_into_address_free_absolute)
                && let Err(error) = lookup(attr, &value)
            {
                checked.external.push(PathFinding::new(path, error));
            }
            checked
        }
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
    lookup: CustomTypeLookup<'_>,
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
                lookup,
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
