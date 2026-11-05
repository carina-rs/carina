//! The only access boundary for a secret's value.
use super::{BindingAddress, DiagnosticScope, InsideSecret, Value, ValueKind};

mod scalar_read {
    pub trait Sealed {}
    impl Sealed for () {}
    impl Sealed for bool {}
    impl Sealed for usize {}
    impl Sealed for i64 {}
    impl<T: Sealed> Sealed for Option<T> {}
}

/// Infallible scalar results permitted by [`SecretPayload::visit`].
/// Error-bearing and value-bearing results must use the protected accessors.
pub trait SecretReadResult: scalar_read::Sealed {}
impl SecretReadResult for () {}
impl SecretReadResult for bool {}
impl SecretReadResult for usize {}
impl SecretReadResult for i64 {}
impl<T: SecretReadResult> SecretReadResult for Option<T> {}

/// A value walker cannot implicitly read or discard the secret wrapper.
///
/// ```compile_fail
/// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress, Value};
/// fn walk(payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
///     let _: &Value = &payload;
/// }
/// ```
///
/// Unprotected errors cannot cross the transformation boundary.
///
/// ```compile_fail
/// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress, Value};
/// struct UnprotectedError(String);
/// fn walk(payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
///     let _: Result<SecretPayload<NoPlaceholder, AbsoluteBindingAddress>, UnprotectedError> =
///         payload.try_map(|value| Err(UnprotectedError(format!("{value:?}"))));
/// }
/// ```
#[derive(Clone, PartialEq)]
pub struct SecretPayload<P: ValueKind, B: BindingAddress> {
    value: Box<Value<P, B>>,
}

impl<P: ValueKind, B: BindingAddress> From<Box<Value<P, B>>> for SecretPayload<P, B> {
    fn from(value: Box<Value<P, B>>) -> Self {
        match *value {
            Value::Deferred(super::DeferredValue::Secret(payload)) => payload,
            value => Self {
                value: Box::new(value),
            },
        }
    }
}

impl<P: ValueKind, B: BindingAddress> SecretPayload<P, B> {
    /// Transform the inner value, protecting errors and collapsing repeated wrappers.
    pub fn try_map<Q: ValueKind, C: BindingAddress, E: InsideSecret>(
        &self,
        map: impl FnOnce(&Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<SecretPayload<Q, C>, E> {
        map(&self.value)
            .map(|value| Box::new(value).into())
            .map_err(InsideSecret::inside_secret)
    }

    /// Inspect a value, protecting every error at the boundary.
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// struct UnprotectedError(String);
    /// fn walk(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     let _ = payload.try_inspect(|value| Err::<(), _>(UnprotectedError(format!("{value:?}"))));
    /// }
    /// ```
    pub fn try_inspect<'a, R, E: InsideSecret>(
        &'a self,
        inspect: impl FnOnce(&'a Value<P, B>) -> Result<R, E>,
    ) -> Result<R, E> {
        inspect(&self.value).map_err(InsideSecret::inside_secret)
    }

    /// Read a scalar without exposing an error channel.
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// fn walk(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     payload.visit(|value| Err::<(), _>(format!("{value:?}")));
    /// }
    /// ```
    pub fn visit<'a, R: SecretReadResult>(&'a self, visit: impl FnOnce(&'a Value<P, B>) -> R) -> R {
        visit(&self.value)
    }

    /// Populate a reference inventory with the scope required by each finding.
    pub(crate) fn visit_reference_inventory<'a>(
        &'a self,
        visit: impl FnOnce(&'a Value<P, B>, DiagnosticScope),
    ) {
        visit(&self.value, DiagnosticScope::Secret);
    }

    /// Explicit escape hatch for transport, persistence, and tests.
    pub fn expose_plaintext(&self) -> &Value<P, B> {
        &self.value
    }
}

impl<P: super::PlaceholderKind> SecretPayload<P, super::AbsoluteBindingAddress> {
    /// Hash protected material, reusing a matching digest when available.
    pub fn hash(
        &self,
        previous: Option<&super::RedactedSecretHash>,
    ) -> Result<super::RedactedSecretHash, crate::value::SerializationError> {
        self.try_inspect(|value| {
            crate::value::canonical_secret_hash_material(value)
                .map(|material| super::RedactedSecretHash::reuse_or_generate(&material, previous))
        })
    }

    /// Verify protected material without exposing serialization diagnostics.
    pub fn verify_hash(
        &self,
        hash: &super::RedactedSecretHash,
    ) -> Result<bool, crate::value::SerializationError> {
        self.try_inspect(|value| {
            crate::value::canonical_secret_hash_material(value)
                .map(|material| hash.verify(material))
        })
    }
}

impl<P: ValueKind, B: BindingAddress> std::fmt::Debug for SecretPayload<P, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("(secret)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{AbsoluteBindingAddress, ConcreteValue, DeferredValue, NoPlaceholder};
    #[test]
    fn review2_f2_payload_collapses_constructor_and_mapping_secrets_with_main_hash_bytes() {
        let value: Value = Value::Concrete(ConcreteValue::String("t".into()));
        let payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> =
            Box::new(value.clone()).into();
        let wrapped = Value::Deferred(DeferredValue::Secret(payload.clone()));
        let constructed: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> =
            Box::new(wrapped.clone()).into();
        assert_eq!(constructed.expose_plaintext(), &value);
        let mapped = payload
            .try_map(|_| Ok::<_, std::convert::Infallible>(wrapped))
            .unwrap();
        assert_eq!(mapped.expose_plaintext(), &value);
        // origin/main hashes the canonical JSON string, without wrapper tags.
        let hash = payload.hash(None).unwrap();
        assert!(hash.verify(br#""t""#));
        assert!(constructed.verify_hash(&hash).unwrap());
        assert_eq!(mapped.hash(Some(&hash)).unwrap(), hash);
    }

    #[test]
    fn secret_origin_4275_payload_protects_errors_and_normalizes_depth() {
        let value: Value = Value::Concrete(ConcreteValue::String("private-payload".into()));
        let inner = Value::Deferred(DeferredValue::Secret(Box::new(value).into()));
        let payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> = Box::new(inner).into();
        let error = payload
            .try_map::<NoPlaceholder, AbsoluteBindingAddress, String>(|value| {
                Err(format!("{value:?} private-error"))
            })
            .unwrap_err();
        assert!(!error.contains("private-"), "{error}");
        let mapped = payload
            .try_map(|value| Ok::<_, std::convert::Infallible>(value.clone()))
            .unwrap();
        assert!(matches!(
            mapped.expose_plaintext(),
            Value::Concrete(ConcreteValue::String(_))
        ));
        payload.visit_reference_inventory(|_, scope| assert_eq!(scope, DiagnosticScope::Secret));
        let error = payload
            .try_inspect(|value| Err::<(), _>(format!("{value:?} private-error")))
            .unwrap_err();
        assert_eq!(error, crate::resource::INSIDE_SECRET_MESSAGE);
        assert!(payload.visit(|value| matches!(value, Value::Concrete(ConcreteValue::String(_)))));
        assert_eq!(payload.try_inspect(|_| Ok::<_, String>(42)).unwrap(), 42);
    }
}
