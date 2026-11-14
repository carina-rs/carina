//! Backend evaluation retains attempted failures in the value, independently
//! of loader mode or the presence of an analysis tree.

use super::{AttributeKey, ConfigurationAttributes, ParseError};
use crate::resource::{
    BindingAddress, NoPlaceholder, ParserPlaceholder, RelativeBindingName, Value, ValueKind,
};
use std::sync::Arc;

/// Only evaluated values can enter backend schema admission. A failed value
/// carries its evaluator's typed, located cause through every loader projection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(
    untagged,
    bound(
        serialize = "Value<P, B>: serde::Serialize",
        deserialize = "Value<P, B>: serde::Deserialize<'de>"
    )
)]
pub enum BackendAttributeValue<
    P: ValueKind = NoPlaceholder,
    B: BindingAddress = RelativeBindingName,
> {
    Evaluated(Value<P, B>),
    #[serde(skip)]
    Failed(Arc<ParseError>),
}

impl<P: ValueKind, B: BindingAddress> BackendAttributeValue<P, B> {
    pub fn evaluated(&self) -> Result<&Value<P, B>, &Arc<ParseError>> {
        match self {
            Self::Evaluated(value) => Ok(value),
            Self::Failed(error) => Err(error),
        }
    }
    pub(crate) fn resolve_bindings<C: BindingAddress>(
        self,
        resolve: &mut impl FnMut(B) -> C,
    ) -> BackendAttributeValue<P, C> {
        match self {
            Self::Evaluated(value) => BackendAttributeValue::Evaluated(value.map_bindings(resolve)),
            Self::Failed(error) => BackendAttributeValue::Failed(error),
        }
    }
}
impl<P: ValueKind> BackendAttributeValue<P, crate::resource::AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        resolve: &mut impl FnMut(
            &crate::resource::AbsoluteBindingAddress,
        ) -> crate::resource::AbsoluteBindingAddress,
    ) {
        if let Self::Evaluated(value) = self {
            value.map_absolute_bindings(resolve);
        }
    }
}
impl<B: BindingAddress> BackendAttributeValue<ParserPlaceholder, B> {
    pub fn into_expanded(
        self,
    ) -> Result<BackendAttributeValue<NoPlaceholder, B>, crate::resource::LeftoverPlaceholder> {
        match self {
            Self::Evaluated(value) => value.into_expanded().map(BackendAttributeValue::Evaluated),
            Self::Failed(error) => Ok(BackendAttributeValue::Failed(error)),
        }
    }
}
impl<P: ValueKind, B: BindingAddress> From<Value<P, B>> for BackendAttributeValue<P, B> {
    fn from(value: Value<P, B>) -> Self {
        Self::Evaluated(value)
    }
}
impl<P: ValueKind, B: BindingAddress, K: Into<AttributeKey>> FromIterator<(K, Value<P, B>)>
    for ConfigurationAttributes<BackendAttributeValue<P, B>>
{
    fn from_iter<T: IntoIterator<Item = (K, Value<P, B>)>>(entries: T) -> Self {
        entries
            .into_iter()
            .map(|(key, value)| (key, BackendAttributeValue::Evaluated(value)))
            .collect()
    }
}
