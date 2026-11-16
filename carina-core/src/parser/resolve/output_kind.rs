//! Test-only storage kind. All evaluation remains in the parent resolver body.
use super::*;
use crate::resource::{NoReferencePayload, SecretPayload, ValueShapeSealed, copy_storage};
use crate::test_utils::resolver_kind::{InputObservation, PathObservation};
use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(super) struct EnrichedAuthored(ParserPlaceholder);
#[derive(Clone)]
pub(super) struct EnrichedReference<B: BindingAddress> {
    path: AccessPath<B>,
    plain_result: Box<S<B>>,
}
#[derive(Clone)]
pub(super) struct EnrichedConversion<B: BindingAddress> {
    operands: Vec<V<EnrichedAuthored, B>>,
    plain_result: Box<S<B>>,
}
// Do not format retained plaintext, even if someone accidentally formats a payload.
impl<B: BindingAddress> fmt::Debug for EnrichedReference<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnrichedReference")
    }
}
impl<B: BindingAddress> fmt::Debug for EnrichedConversion<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnrichedConversion")
    }
}
impl ValueShapeSealed for EnrichedAuthored {}
impl ValueShape for EnrichedAuthored {
    type ReferencePayload<B: BindingAddress> = Arc<EnrichedReference<B>>;
    type ConversionPayload<B: BindingAddress> = Arc<EnrichedConversion<B>>;
}
impl ResolveKind for EnrichedAuthored {
    type Operand<B: BindingAddress> = V<Self, B>;
    fn embed_plain<B: BindingAddress>(value: S<B>) -> V<Self, B> {
        copy_storage(
            &value,
            &mut |p| Self(p.clone()),
            &mut |never: &NoReferencePayload| match *never {},
            &mut |never: &NoReferencePayload| match *never {},
        )
    }
    fn project<B: BindingAddress>(value: &V<Self, B>) -> Cow<'_, S<B>> {
        match value {
            V::Reference(payload) => Cow::Borrowed(&payload.plain_result),
            V::Conversion(payload) => Cow::Borrowed(&payload.plain_result),
            _ => Cow::Owned(copy_storage(
                value,
                &mut |p| p.0.clone(),
                &mut |payload| (*payload.plain_result).clone(),
                &mut |payload| (*payload.plain_result).clone(),
            )),
        }
    }
    fn split_operand<B: BindingAddress>(value: V<Self, B>) -> (S<B>, V<Self, B>) {
        (Self::project(&value).into_owned(), value)
    }
    fn protect_operand<B: BindingAddress>(operand: V<Self, B>) -> V<Self, B> {
        V::Deferred(DeferredValue::Secret(SecretPayload::wrap_storage(operand)))
    }
    fn read<B: BindingAddress>(path: &AccessPath<B>, plain_result: S<B>) -> V<Self, B> {
        V::Reference(Arc::new(EnrichedReference {
            path: path.clone(),
            plain_result: Box::new(plain_result),
        }))
    }
    fn computed<B: BindingAddress>(operands: Vec<V<Self, B>>, plain_result: S<B>) -> V<Self, B> {
        V::Conversion(Arc::new(EnrichedConversion {
            operands,
            plain_result: Box::new(plain_result),
        }))
    }
}

pub(super) fn observe_paths(
    value: &V<EnrichedAuthored, RelativeBindingName>,
) -> Vec<PathObservation> {
    fn walk(
        value: &V<EnrichedAuthored, RelativeBindingName>,
        depth: usize,
        found: &RefCell<Vec<PathObservation>>,
    ) {
        // Reuse the exhaustive storage walk; its return value is irrelevant here.
        let _: S<RelativeBindingName> = copy_storage(
            value,
            &mut |p| p.0.clone(),
            &mut |payload| {
                found.borrow_mut().push(PathObservation {
                    path: payload.path.to_dot_string(),
                    segmented: !payload.path.segments().is_empty(),
                    conversion_depth: depth,
                });
                (*payload.plain_result).clone()
            },
            &mut |payload| {
                for operand in &payload.operands {
                    walk(operand, depth + 1, found);
                }
                (*payload.plain_result).clone()
            },
        );
    }
    let found = RefCell::new(Vec::new());
    walk(value, 0, &found);
    found.into_inner()
}
pub(super) fn input_observation(value: &S<RelativeBindingName>) -> InputObservation {
    let shape = match value {
        S::Reference(never) | S::Conversion(never) => match *never {},
        S::Concrete(ConcreteValue::List(_)) => "List",
        S::Concrete(ConcreteValue::Map(_)) => "Map",
        S::Concrete(_) => "Literal",
        S::Deferred(DeferredValue::ResourceRef { .. }) => "ResourceRef",
        S::Deferred(DeferredValue::Coalesce { .. }) => "Coalesce",
        S::Deferred(DeferredValue::FunctionCall { .. }) => "FunctionCall",
        S::Deferred(DeferredValue::ResolvedFunctionCall { .. }) => "ResolvedFunctionCall",
        S::Deferred(DeferredValue::Interpolation(_)) => "Interpolation",
        S::Deferred(DeferredValue::IntegerArithmetic(_)) => "IntegerArithmetic",
        S::Deferred(DeferredValue::Access(_)) => "Access",
        S::Deferred(DeferredValue::Secret(_)) => "Secret",
        S::Deferred(_) => "OtherDeferred",
    };
    InputObservation {
        shape,
        direct_path: match value {
            S::Deferred(DeferredValue::ResourceRef { path }) => Some(path.to_dot_string()),
            _ => None,
        },
        coalesce_left_deferred: matches!(value,S::Deferred(DeferredValue::Coalesce {value,..}) if matches!(value.as_ref(),S::Deferred(DeferredValue::FunctionCall {..} | DeferredValue::ResolvedFunctionCall {..}))),
    }
}
