//! Test-only storage kind. All evaluation remains in the parent resolver body.
use super::*;
use crate::resource::{NoReferencePayload, SecretPayload, ValueShapeSealed, copy_storage};
use crate::test_utils::resolver_kind::{InputObservation, PathObservation};
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
// Only this module can mint an operand for the unmarked storage constructor.
pub(crate) struct SecretStorageOperand<P: ValueShape, B: BindingAddress>(V<P, B>);
impl<P: ValueShape, B: BindingAddress> SecretStorageOperand<P, B> {
    pub(crate) fn into_value(self) -> V<P, B> {
        self.0
    }
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
        count(Event::ProjectionVisit);
        project_storage(value)
    }
    fn split_operand<B: BindingAddress>(value: V<Self, B>) -> (S<B>, V<Self, B>) {
        (own_projection(Self::project(&value)), value)
    }
    fn protect_operand<B: BindingAddress>(operand: V<Self, B>) -> V<Self, B> {
        V::Deferred(DeferredValue::Secret(SecretPayload::wrap_storage(
            SecretStorageOperand(operand),
        )))
    }
    fn read<B: BindingAddress>(path: &AccessPath<B>, plain_result: S<B>) -> V<Self, B> {
        count(Event::ReferenceEnvelope);
        V::Reference(Arc::new(EnrichedReference {
            path: path.clone(),
            plain_result: Box::new(plain_result),
        }))
    }
    fn computed<B: BindingAddress>(operands: Vec<V<Self, B>>, plain_result: S<B>) -> V<Self, B> {
        count(Event::ConversionEnvelope);
        V::Conversion(Arc::new(EnrichedConversion {
            operands,
            plain_result: Box::new(plain_result),
        }))
    }
}

// A cached result may be arbitrarily deep. Cow::into_owned would use Value's
// recursive Clone here, adding native recursion to the iterative resolver.
fn clone_plain<B: BindingAddress>(value: &S<B>) -> S<B> {
    copy_storage(
        value,
        &mut Clone::clone,
        &mut |never: &NoReferencePayload| match *never {},
        &mut |never: &NoReferencePayload| match *never {},
    )
}

fn own_projection<B: BindingAddress>(value: Cow<'_, S<B>>) -> S<B> {
    match value {
        Cow::Borrowed(value) => clone_plain(value),
        Cow::Owned(value) => value,
    }
}

fn project_storage<B: BindingAddress>(value: &V<EnrichedAuthored, B>) -> Cow<'_, S<B>> {
    match value {
        V::Reference(payload) => Cow::Borrowed(&payload.plain_result),
        V::Conversion(payload) => Cow::Borrowed(&payload.plain_result),
        _ => Cow::Owned(copy_storage(
            value,
            &mut |p| p.0.clone(),
            &mut |payload| clone_plain(&payload.plain_result),
            &mut |payload| clone_plain(&payload.plain_result),
        )),
    }
}

pub(super) fn project_for_comparison<B: BindingAddress>(value: &V<EnrichedAuthored, B>) -> S<B> {
    own_projection(project_storage(value))
}

pub(super) fn observe_paths(
    value: &V<EnrichedAuthored, RelativeBindingName>,
) -> Vec<PathObservation> {
    enum Envelope {
        Reference(Arc<EnrichedReference<RelativeBindingName>>),
        Conversion(Arc<EnrichedConversion<RelativeBindingName>>),
    }
    fn envelopes(value: &V<EnrichedAuthored, RelativeBindingName>, found: &mut Vec<Envelope>) {
        // Use the storage copier's existing envelope callbacks. No additional
        // API exposes a secret's child to an ordinary reader.
        let found = std::cell::RefCell::new(found);
        let _: S<RelativeBindingName> = copy_storage(
            value,
            &mut |p| p.0.clone(),
            &mut |payload| {
                found
                    .borrow_mut()
                    .push(Envelope::Reference(Arc::clone(payload)));
                clone_plain(&payload.plain_result)
            },
            &mut |payload| {
                found
                    .borrow_mut()
                    .push(Envelope::Conversion(Arc::clone(payload)));
                clone_plain(&payload.plain_result)
            },
        );
    }
    let mut initial = Vec::new();
    envelopes(value, &mut initial);
    let mut pending = initial
        .into_iter()
        .rev()
        .map(|node| (node, 0))
        .collect::<Vec<_>>();
    let mut found = Vec::new();
    while let Some((node, depth)) = pending.pop() {
        match node {
            Envelope::Reference(payload) => found.push(PathObservation {
                path: payload.path.to_dot_string(),
                segmented: !payload.path.segments().is_empty(),
                conversion_depth: depth,
            }),
            Envelope::Conversion(payload) => {
                let mut children = Vec::new();
                for operand in &payload.operands {
                    envelopes(operand, &mut children);
                }
                pending.extend(children.into_iter().rev().map(|node| (node, depth + 1)));
            }
        }
    }
    found
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
        S::Deferred(DeferredValue::ProvisionalFunctionCall { .. }) => "ProvisionalFunctionCall",
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
        coalesce_left_deferred: matches!(value,S::Deferred(DeferredValue::Coalesce {value,..}) if matches!(value.as_ref(),S::Deferred(DeferredValue::FunctionCall {..} | DeferredValue::ResolvedFunctionCall {..} | DeferredValue::ProvisionalFunctionCall {..}))),
    }
}
