//! Declaration metadata that can belong to a read value. Member slots do not
//! exist here; materializing a value type always supplies empty member records.

use crate::schema::{
    AttrTypeKind, Compiled, CompiledAttributeType, CompiledPattern, CustomValidator, DslTransform,
    IntIdentityLayer, RefinementBinding, StringIdentityLayer, TypeIdentity,
};

#[derive(Clone)]
pub(super) enum ValueTypeDeclaration {
    String(StringIdentityLayer<Compiled>),
    Int(IntIdentityLayer),
    Float {
        binding: RefinementBinding,
        range: Option<(Option<crate::FiniteF64>, Option<crate::FiniteF64>)>,
    },
    Reference {
        identity: TypeIdentity,
        pattern: Option<CompiledPattern>,
        length: Option<(Option<u64>, Option<u64>)>,
    },
    Enum {
        identity: TypeIdentity,
        base: Box<Self>,
        values: Option<Vec<String>>,
        dsl_aliases: Vec<(String, String)>,
        validate: Option<CustomValidator>,
        to_dsl: Option<DslTransform>,
    },
}

impl std::fmt::Debug for ValueTypeDeclaration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.materialize(), f)
    }
}

impl ValueTypeDeclaration {
    pub(super) fn from_declaration(declaration: &CompiledAttributeType) -> Option<Self> {
        Some(match declaration.kind() {
            AttrTypeKind::String {
                identity_layer,
                member_layer: _,
            } => Self::String(identity_layer.clone()),
            AttrTypeKind::Int {
                identity_layer,
                member_layer: _,
            } => Self::Int(identity_layer.clone()),
            AttrTypeKind::Float { binding, range } => Self::Float {
                binding: binding.clone(),
                range: *range,
            },
            AttrTypeKind::Reference {
                identity,
                pattern,
                length,
            } => Self::Reference {
                identity: identity.clone(),
                pattern: pattern.clone(),
                length: *length,
            },
            AttrTypeKind::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                validate,
                to_dsl,
            } => Self::Enum {
                identity: identity.clone(),
                base: Box::new(Self::from_declaration(base)?),
                values: values.clone(),
                dsl_aliases: dsl_aliases.clone(),
                validate: validate.clone(),
                to_dsl: to_dsl.clone(),
            },
            _ => return None,
        })
    }

    pub(super) fn materialize(&self) -> CompiledAttributeType {
        let kind = match self {
            Self::String(identity_layer) => AttrTypeKind::String {
                identity_layer: identity_layer.clone(),
                member_layer: Default::default(),
            },
            Self::Int(identity_layer) => AttrTypeKind::Int {
                identity_layer: identity_layer.clone(),
                member_layer: Default::default(),
            },
            Self::Float { binding, range } => AttrTypeKind::Float {
                binding: binding.clone(),
                range: *range,
            },
            Self::Reference {
                identity,
                pattern,
                length,
            } => AttrTypeKind::Reference {
                identity: identity.clone(),
                pattern: pattern.clone(),
                length: *length,
            },
            Self::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                validate,
                to_dsl,
            } => AttrTypeKind::Enum {
                identity: identity.clone(),
                base: Box::new(base.materialize()),
                values: values.clone(),
                dsl_aliases: dsl_aliases.clone(),
                validate: validate.clone(),
                to_dsl: to_dsl.clone(),
            },
        };
        CompiledAttributeType { kind }
    }

    pub(super) fn fingerprint(&self) -> String {
        self.materialize().declaration_fingerprint()
    }
}
