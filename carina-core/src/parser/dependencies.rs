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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexpandedDependencies<B: BindingAddress = RelativeBindingName>(
    Vec<UnexpandedDependencyOperand<B>>,
);

impl<B: BindingAddress> Default for UnexpandedDependencies<B> {
    fn default() -> Self {
        Self(Vec::new())
    }
}
impl<B: BindingAddress> UnexpandedDependencies<B> {
    pub fn new(operands: Vec<UnexpandedDependencyOperand<B>>) -> Self {
        Self(operands)
    }
    pub fn iter(&self) -> impl Iterator<Item = &UnexpandedDependencyOperand<B>> {
        self.0.iter()
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut UnexpandedDependencyOperand<B>> {
        self.0.iter_mut()
    }
    pub fn as_slice(&self) -> &[UnexpandedDependencyOperand<B>] {
        &self.0
    }
    pub fn push(&mut self, operand: UnexpandedDependencyOperand<B>) {
        self.0.push(operand);
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
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
        UnexpandedDependencies::new(
            self.into_iter()
                .map(|operand| operand.map_bindings(map))
                .collect(),
        )
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
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
impl<'a, B: BindingAddress> IntoIterator for &'a UnexpandedDependencies<B> {
    type Item = &'a UnexpandedDependencyOperand<B>;
    type IntoIter = std::slice::Iter<'a, UnexpandedDependencyOperand<B>>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<B: BindingAddress> FromIterator<B> for UnexpandedDependencies<B> {
    fn from_iter<T: IntoIterator<Item = B>>(bindings: T) -> Self {
        Self::new(
            bindings
                .into_iter()
                .map(UnexpandedDependencyOperand::BareBinding)
                .collect(),
        )
    }
}
impl<B: BindingAddress> From<Vec<B>> for UnexpandedDependencies<B> {
    fn from(bindings: Vec<B>) -> Self {
        bindings.into_iter().collect()
    }
}
