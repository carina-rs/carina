//! Dependency operands retain their binding phase until graph resolution.
use crate::eval_value::UnexpandedResourceHandle;
use crate::resource::{AbsoluteBindingAddress, BindingAddress, RelativeBindingName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnexpandedDependencyOperand<B: BindingAddress = RelativeBindingName> {
    Handle(UnexpandedResourceHandle<B>),
    BareBinding(B),
    AttributeSelection { binding: B, attribute: String },
}

impl<B: BindingAddress> UnexpandedDependencyOperand<B> {
    pub(crate) fn diagnostic_reference(&self) -> String {
        match self {
            Self::Handle(handle) => handle.diagnostic_name().to_owned(),
            Self::BareBinding(binding) => binding.to_string(),
            Self::AttributeSelection { binding, attribute } => format!("{binding}.{attribute}"),
        }
    }
    pub fn binding_reference(&self) -> B {
        match self {
            Self::Handle(handle) => handle.canonical_binding().clone(),
            Self::BareBinding(binding) => binding.clone(),
            Self::AttributeSelection { binding, attribute } => {
                crate::resource::binding_address::binding_selected_attribute(binding, attribute)
            }
        }
    }
    pub(crate) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> UnexpandedDependencyOperand<C> {
        match self {
            Self::Handle(handle) => UnexpandedDependencyOperand::Handle(handle.map_bindings(map)),
            Self::BareBinding(binding) => UnexpandedDependencyOperand::BareBinding(map(binding)),
            Self::AttributeSelection { binding, attribute } => {
                UnexpandedDependencyOperand::AttributeSelection {
                    binding: map(binding),
                    attribute,
                }
            }
        }
    }
}

/// Authored operands never enter saved JSON:
///
/// ```compile_fail,E0277
/// use carina_core::parser::UnexpandedDependencies;
/// let operands: UnexpandedDependencies = UnexpandedDependencies::default();
/// let _ = serde_json::to_string(&operands);
/// ```
///
/// Operand construction is parser-owned and always retains a location:
///
/// ```compile_fail
/// use carina_core::parser::{UnexpandedDependencies, UnexpandedDependencyOperand};
/// let _: UnexpandedDependencies = UnexpandedDependencies::new(vec![]);
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::parser::UnexpandedDependencies;
/// let _: UnexpandedDependencies = UnexpandedDependencies::from_located(vec![]);
/// ```
///
/// ```compile_fail
/// use carina_core::parser::{UnexpandedDependencies, UnexpandedDependencyOperand};
/// let mut dependencies: UnexpandedDependencies = Default::default();
/// dependencies.push(UnexpandedDependencyOperand::BareBinding("arbitrary".into()));
/// ```
///
/// ```compile_fail
/// use carina_core::parser::{RelativeBindingName, UnexpandedDependencies};
/// let _: UnexpandedDependencies = vec![RelativeBindingName::new("arbitrary")].into();
/// ```
///
/// ```compile_fail
/// use carina_core::parser::{RelativeBindingName, UnexpandedDependencies};
/// let _: UnexpandedDependencies = [RelativeBindingName::new("arbitrary")].into_iter().collect();
/// ```
#[derive(Debug, Clone)]
pub struct UnexpandedDependencies<B: BindingAddress = RelativeBindingName> {
    operands: Vec<super::Located<UnexpandedDependencyOperand<B>>>,
    // Value-reference origins survive eager value resolution alongside the
    // explicit operands. They do not contribute to len/is_empty or semantics.
    reference_origins: Vec<crate::resource::AccessPath<B>>,
}

impl<B: BindingAddress> Default for UnexpandedDependencies<B> {
    fn default() -> Self {
        Self {
            operands: Vec::new(),
            reference_origins: Vec::new(),
        }
    }
}
impl<B: BindingAddress> UnexpandedDependencies<B> {
    pub(in crate::parser) fn from_located(
        operands: Vec<super::Located<UnexpandedDependencyOperand<B>>>,
    ) -> Self {
        Self {
            operands,
            reference_origins: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(operands: Vec<UnexpandedDependencyOperand<B>>) -> Self {
        let parsed = super::parse_file(
            "let operand = 1",
            std::path::Path::new("test.crn"),
            &super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let location = parsed.parser_binding_metadata.syntax().declarations()[0].location();
        Self::from_located(
            operands
                .into_iter()
                .map(|operand| super::Located::new(location.clone(), operand))
                .collect(),
        )
    }

    pub(crate) fn reference_origins(&self) -> &[crate::resource::AccessPath<B>] {
        &self.reference_origins
    }
    pub(crate) fn set_reference_origins(&mut self, origins: Vec<crate::resource::AccessPath<B>>) {
        self.reference_origins = origins;
    }
    pub(in crate::parser) fn record_reference_origins<P: crate::resource::ValueKind>(
        &mut self,
        attributes: &indexmap::IndexMap<String, crate::resource::Value<P, B>>,
    ) {
        for value in attributes.values() {
            value.visit_resource_refs(&mut |path| self.reference_origins.push(path.clone()));
        }
    }

    pub(crate) fn located_operands(&self) -> &[super::Located<UnexpandedDependencyOperand<B>>] {
        &self.operands
    }

    // Expansion may drop a forwarded operand, so transform its provenance in
    // the same operation rather than indexing the pre-expansion source list.
    pub(crate) fn try_filter_map<C: BindingAddress, E>(
        &self,
        mut map: impl FnMut(
            usize,
            &super::Located<UnexpandedDependencyOperand<B>>,
        ) -> Result<Option<UnexpandedDependencyOperand<C>>, E>,
    ) -> Result<UnexpandedDependencies<C>, E> {
        let mut operands = Vec::new();
        for (index, located) in self.operands.iter().enumerate() {
            if let Some(operand) = map(index, located)? {
                operands.push(super::Located::new(located.location().clone(), operand));
            }
        }
        Ok(UnexpandedDependencies {
            operands,
            reference_origins: Vec::new(),
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &UnexpandedDependencyOperand<B>> {
        self.operands.iter().map(super::Located::detail)
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut UnexpandedDependencyOperand<B>> {
        self.operands.iter_mut().map(|operand| operand.detail_mut())
    }
    pub fn is_empty(&self) -> bool {
        self.operands.is_empty()
    }
    pub fn len(&self) -> usize {
        self.operands.len()
    }
    #[cfg(any(test, feature = "test-utils", feature = "test-support"))]
    pub fn binding_references(&self) -> Vec<B> {
        self.iter()
            .map(UnexpandedDependencyOperand::binding_reference)
            .collect()
    }
    #[cfg(any(test, feature = "test-utils", feature = "test-support"))]
    pub fn diagnostic_references(&self) -> Vec<String> {
        self.iter()
            .map(UnexpandedDependencyOperand::diagnostic_reference)
            .collect()
    }
    pub(crate) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> UnexpandedDependencies<C> {
        UnexpandedDependencies {
            operands: self
                .operands
                .into_iter()
                .map(|operand| {
                    let (location, operand) = operand.into_parts();
                    super::Located::new(location, operand.map_bindings(map))
                })
                .collect(),
            reference_origins: self
                .reference_origins
                .into_iter()
                .map(|path| path.map_bindings(map))
                .collect(),
        }
    }
}
impl UnexpandedDependencies<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> UnexpandedDependencies<AbsoluteBindingAddress> {
        self.map_bindings(resolve)
    }
}
impl UnexpandedDependencies<AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        *self = std::mem::take(self).map_bindings(&mut |binding| map(&binding));
    }
}
impl<B: BindingAddress> IntoIterator for UnexpandedDependencies<B> {
    type Item = UnexpandedDependencyOperand<B>;
    type IntoIter = std::iter::Map<
        std::vec::IntoIter<super::Located<Self::Item>>,
        fn(super::Located<Self::Item>) -> Self::Item,
    >;
    fn into_iter(self) -> Self::IntoIter {
        self.operands
            .into_iter()
            .map(|operand| operand.into_parts().1)
    }
}
impl<'a, B: BindingAddress> IntoIterator for &'a UnexpandedDependencies<B> {
    type Item = &'a UnexpandedDependencyOperand<B>;
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, super::Located<UnexpandedDependencyOperand<B>>>,
        fn(&'a super::Located<UnexpandedDependencyOperand<B>>) -> Self::Item,
    >;
    fn into_iter(self) -> Self::IntoIter {
        self.operands.iter().map(super::Located::detail)
    }
}

#[cfg(test)]
impl<B: BindingAddress> FromIterator<B> for UnexpandedDependencies<B> {
    fn from_iter<T: IntoIterator<Item = B>>(bindings: T) -> Self {
        Self::new_for_test(
            bindings
                .into_iter()
                .map(UnexpandedDependencyOperand::BareBinding)
                .collect(),
        )
    }
}
#[cfg(test)]
impl<B: BindingAddress> From<Vec<B>> for UnexpandedDependencies<B> {
    fn from(bindings: Vec<B>) -> Self {
        bindings.into_iter().collect()
    }
}

impl<B: BindingAddress> PartialEq for UnexpandedDependencies<B> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}
impl<B: BindingAddress> Eq for UnexpandedDependencies<B> {}
