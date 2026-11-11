//! Identity definitions and one-shot member restrictions are different types.

use super::*;

/// The complete reusable String definition. Member construction preserves it.
#[derive(Clone)]
pub struct StringIdentityLayer<R: PatternForm = Draft> {
    pub(crate) binding: RefinementBinding,
    pub(crate) pattern: Option<R::Pattern>,
    pub(crate) length: Option<(Option<u64>, Option<u64>)>,
    pub(crate) to_dsl: Option<DslTransform>,
}

/// Exhaustive identity-only projection; it cannot expose member restrictions.
#[derive(Clone, Copy)]
pub struct StringIdentityParts<'a> {
    pub identity: Option<&'a TypeIdentity>,
    pub pattern: Option<&'a PatternSpec>,
    pub length: Option<(Option<u64>, Option<u64>)>,
    pub validate: Option<&'a CustomValidator>,
    pub to_dsl: Option<&'a DslTransform>,
}

impl<R: PatternForm> StringIdentityLayer<R> {
    pub fn new(
        identity: Option<TypeIdentity>,
        pattern: Option<R::Pattern>,
        length: Option<(Option<u64>, Option<u64>)>,
        to_dsl: Option<DslTransform>,
    ) -> Self {
        Self {
            binding: RefinementBinding::MetadataOnly(identity),
            pattern,
            length,
            to_dsl,
        }
    }

    pub fn with_validator(
        identity: TypeIdentity,
        pattern: Option<R::Pattern>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        to_dsl: Option<DslTransform>,
    ) -> Self {
        Self {
            binding: RefinementBinding::Validator {
                identity,
                callback: validate,
            },
            pattern,
            length,
            to_dsl,
        }
    }

    pub fn parts(&self) -> StringIdentityParts<'_> {
        StringIdentityParts {
            identity: self.binding.identity(),
            pattern: self.pattern.as_ref().map(AsRef::as_ref),
            length: self.length,
            validate: self.binding.callback(),
            to_dsl: self.to_dsl.as_ref(),
        }
    }
}

/// Additional declaration-local String restrictions, applied after the identity.
#[derive(Clone, Debug)]
pub struct StringMemberConstraints<R: PatternForm = Draft> {
    pub(crate) pattern: Option<R::Pattern>,
    pub(crate) length: Option<(Option<u64>, Option<u64>)>,
}

#[derive(Clone, Copy, Debug)]
pub struct StringMemberParts<'a> {
    pub pattern: Option<&'a PatternSpec>,
    pub length: Option<(Option<u64>, Option<u64>)>,
}

impl<R: PatternForm> Default for StringMemberConstraints<R> {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl<R: PatternForm> StringMemberConstraints<R> {
    pub fn new(pattern: Option<R::Pattern>, length: Option<(Option<u64>, Option<u64>)>) -> Self {
        Self { pattern, length }
    }

    pub fn parts(&self) -> StringMemberParts<'_> {
        StringMemberParts {
            pattern: self.pattern.as_ref().map(AsRef::as_ref),
            length: self.length,
        }
    }
}

#[derive(Clone)]
pub struct IntIdentityLayer {
    pub(crate) binding: RefinementBinding,
    pub(crate) range: Option<(Option<i64>, Option<i64>)>,
}

#[derive(Clone, Copy)]
pub struct IntIdentityParts<'a> {
    pub identity: Option<&'a TypeIdentity>,
    pub range: Option<(Option<i64>, Option<i64>)>,
    pub validate: Option<&'a CustomValidator>,
}

impl IntIdentityLayer {
    pub fn new(identity: Option<TypeIdentity>, range: Option<(Option<i64>, Option<i64>)>) -> Self {
        Self {
            binding: RefinementBinding::MetadataOnly(identity),
            range,
        }
    }

    pub fn with_validator(
        identity: TypeIdentity,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: CustomValidator,
    ) -> Self {
        Self {
            binding: RefinementBinding::Validator {
                identity,
                callback: validate,
            },
            range,
        }
    }

    pub fn parts(&self) -> IntIdentityParts<'_> {
        IntIdentityParts {
            identity: self.binding.identity(),
            range: self.range,
            validate: self.binding.callback(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IntMemberConstraints {
    pub(crate) range: Option<(Option<i64>, Option<i64>)>,
}

#[derive(Clone, Copy, Debug)]
pub struct IntMemberParts {
    pub range: Option<(Option<i64>, Option<i64>)>,
}

impl IntMemberConstraints {
    pub fn new(range: Option<(Option<i64>, Option<i64>)>) -> Self {
        Self { range }
    }
    pub fn parts(&self) -> IntMemberParts {
        IntMemberParts { range: self.range }
    }
}

/// The only input accepted by identity consistency, signatures and built-ins.
/// Every variant excludes declaration-local member constraints.
#[derive(Clone, Copy)]
pub(crate) enum ScalarIdentityParts<'a> {
    String(StringIdentityParts<'a>),
    Int(IntIdentityParts<'a>),
    Float {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<crate::FiniteF64>, Option<crate::FiniteF64>)>,
        validate: Option<&'a CustomValidator>,
    },
}

impl<'a> ScalarIdentityParts<'a> {
    pub(crate) fn callback(self) -> Option<&'a CustomValidator> {
        match self {
            Self::String(parts) => parts.validate,
            Self::Int(parts) => parts.validate,
            Self::Float { validate, .. } => validate,
        }
    }

    /// Compare reusable identity metadata without allocating. Callbacks retain
    /// owner-local semantic dispatch and have no portable behavior identity.
    pub(crate) fn agrees_with(self, other: Self) -> bool {
        match (self, other) {
            (
                Self::String(StringIdentityParts {
                    identity: left_identity,
                    pattern: left_pattern,
                    length: left_length,
                    validate: _,
                    to_dsl: left_to_dsl,
                }),
                Self::String(StringIdentityParts {
                    identity: right_identity,
                    pattern: right_pattern,
                    length: right_length,
                    validate: _,
                    to_dsl: right_to_dsl,
                }),
            ) => {
                left_identity == right_identity
                    && left_pattern == right_pattern
                    && left_length == right_length
                    && left_to_dsl == right_to_dsl
            }
            (
                Self::Int(IntIdentityParts {
                    identity: left_identity,
                    range: left_range,
                    validate: _,
                }),
                Self::Int(IntIdentityParts {
                    identity: right_identity,
                    range: right_range,
                    validate: _,
                }),
            ) => left_identity == right_identity && left_range == right_range,
            (
                Self::Float {
                    identity: left,
                    range: left_range,
                    validate: _,
                },
                Self::Float {
                    identity: right,
                    range: right_range,
                    validate: _,
                },
            ) => left == right && left_range == right_range,
            _ => false,
        }
    }

    /// Built-in and aggregate admission compare the same identity metadata.
    pub(crate) fn same_definition(self, other: Self) -> bool {
        self.agrees_with(other)
    }
}

impl<R: PatternForm> PatternAttributeType<R> {
    /// Construct a member once, preserving the complete supplied identity.
    ///
    /// ```compile_fail
    /// use carina_core::schema::{AttributeType, StringMemberConstraints};
    /// let local = StringMemberConstraints::new(None, None);
    /// let _ = AttributeType::string_member(local.clone(), local);
    /// ```
    pub fn string_member(
        identity_layer: StringIdentityLayer<R>,
        member_layer: StringMemberConstraints<R>,
    ) -> Self {
        Self {
            kind: AttrTypeKind::String {
                identity_layer,
                member_layer,
            },
        }
    }

    pub fn int_member(
        identity_layer: IntIdentityLayer,
        member_layer: IntMemberConstraints,
    ) -> Self {
        Self {
            kind: AttrTypeKind::Int {
                identity_layer,
                member_layer,
            },
        }
    }

    pub fn string_identity_layer(&self) -> Option<&StringIdentityLayer<R>> {
        match &self.kind {
            AttrTypeKind::String { identity_layer, .. } => Some(identity_layer),
            _ => None,
        }
    }

    pub(crate) fn has_empty_refinement_bounds(&self) -> bool {
        match &self.kind {
            AttrTypeKind::String {
                identity_layer,
                member_layer,
            } => bounds_are_empty(intersect_bounds(identity_layer.length, member_layer.length)),
            AttrTypeKind::Int {
                identity_layer,
                member_layer,
            } => bounds_are_empty(intersect_bounds(identity_layer.range, member_layer.range)),
            _ => false,
        }
    }

    pub(crate) fn identity_only_declaration(&self) -> Self {
        match &self.kind {
            AttrTypeKind::String { identity_layer, .. } => {
                Self::string_member(identity_layer.clone(), StringMemberConstraints::default())
            }
            AttrTypeKind::Int { identity_layer, .. } => {
                Self::int_member(identity_layer.clone(), IntMemberConstraints::default())
            }
            _ => self.clone(),
        }
    }

    pub(crate) fn scalar_identity_parts(&self) -> Option<ScalarIdentityParts<'_>> {
        match &self.kind {
            AttrTypeKind::String { identity_layer, .. } => {
                Some(ScalarIdentityParts::String(identity_layer.parts()))
            }
            AttrTypeKind::Int { identity_layer, .. } => {
                Some(ScalarIdentityParts::Int(identity_layer.parts()))
            }
            AttrTypeKind::Float { binding, range } => Some(ScalarIdentityParts::Float {
                identity: binding.identity(),
                range: *range,
                validate: binding.callback(),
            }),
            _ => None,
        }
    }
}

pub(crate) fn bounds_are_empty<T: Copy + Ord>(bounds: Option<(Option<T>, Option<T>)>) -> bool {
    matches!(bounds, Some((Some(min), Some(max))) if min > max)
}

pub fn intersect_bounds<T: Copy + Ord>(
    identity: Option<(Option<T>, Option<T>)>,
    member: Option<(Option<T>, Option<T>)>,
) -> Option<(Option<T>, Option<T>)> {
    match (identity, member) {
        (None, bounds) | (bounds, None) => bounds,
        (Some((a_min, a_max)), Some((b_min, b_max))) => Some((
            a_min.into_iter().chain(b_min).max(),
            a_max.into_iter().chain(b_max).min(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named() -> StringIdentityLayer {
        StringIdentityLayer::new(
            Some(TypeIdentity::bare("Name")),
            Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
            Some((Some(1), Some(32))),
            None,
        )
    }

    #[test]
    fn identity_agreement_excludes_members_and_full_fingerprints_include_them() {
        let schema = ValidatedConfigSchema::validate(BTreeMap::from([
            (
                "first".into(),
                AttributeType::string_member(
                    named(),
                    StringMemberConstraints::new(None, Some((Some(2), Some(16)))),
                ),
            ),
            (
                "second".into(),
                AttributeType::string_member(
                    named(),
                    StringMemberConstraints::new(
                        Some(PatternSpec::new("a.*", PatternMatch::Full)),
                        Some((Some(3), Some(20))),
                    ),
                ),
            ),
        ]))
        .unwrap();
        let first = &schema.parts()["first"];
        let second = &schema.parts()["second"];
        assert!(
            first
                .scalar_identity_parts()
                .unwrap()
                .same_definition(second.scalar_identity_parts().unwrap())
        );
        assert_ne!(
            first.declaration_fingerprint(),
            second.declaration_fingerprint()
        );
        assert!(
            ValidatedProviderSchemaWitness::new(vec![], schema, BTreeMap::new(), vec![]).is_ok()
        );
    }

    #[test]
    fn aggregate_admission_rejects_different_identity_constraints_across_roots() {
        let resource = ResourceSchema::new("Names")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::string_member(named(), StringMemberConstraints::default()),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let different = AttributeType::refined_string(
            Some(TypeIdentity::bare("Name")),
            Some(PatternSpec::new("[A-Z]+", PatternMatch::Full)),
            Some((Some(1), Some(32))),
            None,
        );
        let config =
            ValidatedConfigSchema::validate(BTreeMap::from([("value".into(), different)])).unwrap();
        assert!(matches!(
            ValidatedProviderSchemaWitness::new(vec![resource], config, BTreeMap::new(), vec![]),
            Err(ProviderSchemaValidationError::ConflictingIdentityDeclarations { .. })
        ));
    }

    fn reference(pattern: &str, min: u64, max: u64) -> AttributeType {
        AttributeType::reference(
            TypeIdentity::bare("KeyReference"),
            Some(PatternSpec::new(pattern, PatternMatch::Full)),
            Some((Some(min), Some(max))),
        )
    }

    fn admit_across_roots(
        resource_type: AttributeType,
        config_type: AttributeType,
    ) -> Result<ValidatedProviderSchemaWitness, ProviderSchemaValidationError> {
        let resource = ResourceSchema::new("Keys")
            .attribute(AttributeSchema::optional("key", resource_type).build())
            .validate()
            .unwrap();
        let config =
            ValidatedConfigSchema::validate(BTreeMap::from([("key".into(), config_type)])).unwrap();
        ValidatedProviderSchemaWitness::new(vec![resource], config, BTreeMap::new(), vec![])
    }

    #[test]
    fn aggregate_admission_reference_rejects_different_lengths() {
        assert!(matches!(
            admit_across_roots(reference("[a-z]+", 1, 2048), reference("[a-z]+", 0, 256)),
            Err(ProviderSchemaValidationError::ConflictingIdentityDeclarations { identity })
                if identity == TypeIdentity::bare("KeyReference")
        ));
    }

    #[test]
    fn aggregate_admission_reference_rejects_different_patterns() {
        assert!(matches!(
            admit_across_roots(reference("[a-z]+", 1, 2048), reference("[A-Z]+", 1, 2048)),
            Err(ProviderSchemaValidationError::ConflictingIdentityDeclarations { identity })
                if identity == TypeIdentity::bare("KeyReference")
        ));
    }

    #[test]
    fn aggregate_admission_reference_accepts_identical_definitions() {
        assert!(
            admit_across_roots(reference("[a-z]+", 1, 2048), reference("[a-z]+", 1, 2048)).is_ok()
        );
    }
}
