//! The only access boundary for a secret's value.
use super::{BindingAddress, DiagnosticScope, InsideSecret, Value, ValueKind};

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
        Self { value }
    }
}

impl<P: ValueKind, B: BindingAddress> SecretPayload<P, B> {
    /// Transform one layer, protecting errors and preserving the wrapper depth.
    pub fn try_map<Q: ValueKind, C: BindingAddress, E: InsideSecret>(
        &self,
        map: impl FnOnce(&Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<SecretPayload<Q, C>, E> {
        map(&self.value)
            .map(|value| SecretPayload {
                value: Box::new(value),
            })
            .map_err(InsideSecret::inside_secret)
    }

    /// Inspect a value with the diagnostic scope that must accompany findings.
    pub fn visit<'a>(&'a self, visit: impl FnOnce(&'a Value<P, B>, DiagnosticScope)) {
        visit(&self.value, DiagnosticScope::Secret);
    }

    /// Explicit escape hatch for hashing, transport, persistence, and tests.
    pub fn expose_plaintext(&self) -> &Value<P, B> {
        &self.value
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
    fn secret_origin_4275_payload_protects_errors_and_preserves_depth() {
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
            Value::Deferred(DeferredValue::Secret(_))
        ));
        payload.visit(|_, scope| assert_eq!(scope, DiagnosticScope::Secret));
    }
}
