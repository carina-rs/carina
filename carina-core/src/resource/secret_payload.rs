//! The only access boundary for a secret's value.
use super::{BindingAddress, DiagnosticScope, InsideSecret, Value, ValueKind};

mod scalar_read {
    pub trait Sealed {}
    impl Sealed for () {}
    impl Sealed for bool {}
    impl Sealed for usize {}
    impl Sealed for i64 {}
    impl<T: Sealed> Sealed for Option<T> {}
    impl<P: super::ValueKind, B: super::BindingAddress> Sealed for super::SecretPayload<P, B> {}
    impl Sealed for super::super::RedactedSecretHash {}
}
pub(crate) use scalar_read::Sealed as SealedSecretReadResult;

/// Scalar and protected results permitted by [`SecretPayload::visit`] and
/// [`SecretPayload::try_inspect`]. Plaintext requires a named projection.
pub trait SecretReadResult: scalar_read::Sealed {}
impl SecretReadResult for () {}
impl SecretReadResult for bool {}
impl SecretReadResult for usize {}
impl SecretReadResult for i64 {}
impl<T: SecretReadResult> SecretReadResult for Option<T> {}
impl<P: ValueKind, B: BindingAddress> SecretReadResult for SecretPayload<P, B> {}
impl SecretReadResult for super::RedactedSecretHash {}

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
            mut value => {
                mark_strings(&mut value);
                Self {
                    value: Box::new(value),
                }
            }
        }
    }
}

/// Attach diagnostic provenance once, without evaluating or changing value shapes.
fn mark_strings<P: ValueKind, B: BindingAddress>(value: &mut Value<P, B>) {
    use super::{ConcreteValue, DeferredValue, InterpolationPart, PathSegment, Subscript};
    fn mark_step(step: &mut PathSegment) {
        if let PathSegment::Subscript {
            index: Subscript::Str { key },
        } = step
        {
            key.diagnostic_sensitive = true;
        }
    }
    match value {
        Value::Concrete(ConcreteValue::String(text)) => text.diagnostic_sensitive = true,
        Value::Concrete(ConcreteValue::StringList(texts)) => {
            for text in texts {
                text.diagnostic_sensitive = true;
            }
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            for value in values {
                mark_strings(value);
            }
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            for value in values.values_mut() {
                mark_strings(value);
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                match part {
                    InterpolationPart::Literal(text) => text.diagnostic_sensitive = true,
                    InterpolationPart::Expr(value) => mark_strings(value),
                }
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. },
        ) => {
            for arg in args {
                *arg = arg
                    .try_map_value(|value| {
                        let mut value = value.clone();
                        mark_strings(&mut value);
                        Ok::<_, std::convert::Infallible>(value)
                    })
                    .unwrap();
            }
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            mark_strings(value);
            mark_strings(default);
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            mark_strings(&mut access.base);
            mark_step(&mut access.step);
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            for step in &mut path.segments {
                mark_step(step);
            }
        }
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            **deferred = deferred.map(|value| {
                let mut value = value.clone();
                mark_strings(&mut value);
                value
            });
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            *expression = expression
                .clone()
                .try_map_stage(|mut value| {
                    mark_strings(&mut value);
                    Ok::<_, std::convert::Infallible>(value)
                })
                .unwrap();
        }
        // A nested payload has already crossed this constructor.
        Value::Deferred(
            DeferredValue::Secret(_)
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        )
        | Value::Concrete(
            ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_),
        ) => {}
    }
}

impl<P: ValueKind, B: BindingAddress> SecretPayload<P, B> {
    /// Schema alias normalization mutates the payload without copying recursive
    /// subtrees or removing its protection wrapper.
    pub(crate) fn canonicalize_schema_aliases(
        &mut self,
        canonicalize: impl FnOnce(&mut Value<P, B>),
    ) {
        canonicalize(&mut self.value);
    }
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
    /// Plaintext cannot leave through the success channel, either borrowed or owned.
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// fn extract(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     let _ = payload.try_inspect(|value| Ok::<_, std::convert::Infallible>(value));
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// fn extract(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     let _ = payload.try_inspect(|value| Ok::<_, std::convert::Infallible>(value.clone()));
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// fn extract(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     let _ = payload.try_inspect(|_| Ok::<_, std::convert::Infallible>("plaintext".to_owned()));
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// fn extract(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     let _ = payload.try_inspect(|_| Ok::<_, std::convert::Infallible>(serde_json::json!("plaintext")));
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// struct UnprotectedError(String);
    /// fn walk(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     let _ = payload.try_inspect(|value| Err::<(), _>(UnprotectedError(format!("{value:?}"))));
    /// }
    /// ```
    pub fn try_inspect<'a, R: SecretReadResult, E: InsideSecret>(
        &'a self,
        inspect: impl FnOnce(&'a Value<P, B>) -> Result<R, E>,
    ) -> Result<R, E> {
        inspect(&self.value).map_err(InsideSecret::inside_secret)
    }

    /// Project plaintext JSON for a transport boundary, protecting conversion errors.
    /// The returned JSON contains secret material and must go only to that transport.
    pub fn project_plaintext_json_for_transport<E: InsideSecret>(
        &self,
        project: impl FnOnce(&Value<P, B>) -> Result<serde_json::Value, E>,
    ) -> Result<serde_json::Value, E> {
        self.try_project_plaintext(project)
    }

    /// Explicit plaintext boundary for core serialization, evaluation and schema proofs.
    /// Each caller owns the resulting material and its protection or rewrapping.
    pub(crate) fn try_project_plaintext<'a, R, E: InsideSecret>(
        &'a self,
        project: impl FnOnce(&'a Value<P, B>) -> Result<R, E>,
    ) -> Result<R, E> {
        project(&self.value).map_err(InsideSecret::inside_secret)
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
    pub(crate) fn visit_plaintext_reference_inventory<'a>(
        &'a self,
        visit: impl FnOnce(&'a Value<P, B>, DiagnosticScope),
    ) {
        visit(&self.value, DiagnosticScope::Secret);
    }

    /// Feed plaintext into the internal comparison hasher without exposing a value.
    pub(crate) fn hash_plaintext_into(&self, hasher: &mut impl std::hash::Hasher) {
        self.value.hash_into(hasher);
    }

    /// Explicit escape hatch for transport and persistence.
    ///
    /// ```compile_fail
    /// use carina_core::resource::{SecretPayload, NoPlaceholder, AbsoluteBindingAddress};
    /// fn extract(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
    ///     let _ = payload.expose_plaintext();
    /// }
    /// ```
    pub(crate) fn expose_plaintext(&self) -> &Value<P, B> {
        &self.value
    }

    /// Inspect runtime shapes in cross-crate test fixtures.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn plaintext_for_test(&self) -> &Value<P, B> {
        self.expose_plaintext()
    }
}

impl<P: super::PlaceholderKind> SecretPayload<P, super::AbsoluteBindingAddress> {
    /// Hash protected material, reusing a matching digest when available.
    pub fn hash(
        &self,
        previous: Option<&super::RedactedSecretHash>,
    ) -> Result<super::RedactedSecretHash, crate::value::SerializationError> {
        #[cfg(feature = "test-utils")]
        let _recording = crate::identity_recording::payload_hash_context();
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
        f.write_str(crate::schema::SECRET_VALUE_MASK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{AbsoluteBindingAddress, ConcreteValue, DeferredValue, NoPlaceholder};
    #[test]
    fn secret_origin_4275_inspection_returns_only_scalars_or_protected_values() {
        let payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> = Box::new(
            Value::Concrete(ConcreteValue::String("private-transport".into())),
        )
        .into();
        let protected: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> = payload
            .try_inspect(|value| Ok::<_, std::convert::Infallible>(Box::new(value.clone()).into()))
            .unwrap();
        assert_eq!(protected, payload);
        assert_eq!(format!("{protected:?}"), "(secret)");
        assert!(
            payload
                .try_inspect(|value| Ok::<_, String>(matches!(
                    (value)
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .map(|proof| proof.into_classification()),
                    Ok(crate::resource::knowledge::Classification::Known(_))
                )))
                .unwrap()
        );
        assert_eq!(
            payload
                .try_inspect(|_| Ok::<_, String>(Some(2_usize)))
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            payload
                .project_plaintext_json_for_transport(crate::value::value_to_json)
                .unwrap(),
            serde_json::json!("private-transport")
        );
        let error = payload
            .project_plaintext_json_for_transport(|_| {
                Err::<serde_json::Value, _>("private-error".to_string())
            })
            .unwrap_err();
        assert_eq!(error, super::super::INSIDE_SECRET_MESSAGE);
    }
    #[test]
    fn review3_payload_marking_preserves_equality_hash_material_and_wire() {
        use std::hash::{Hash, Hasher};
        let plain: Value = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
            (
                "text".into(),
                Value::Concrete(ConcreteValue::String("private-runtime".into())),
            ),
            (
                "list".into(),
                Value::Concrete(ConcreteValue::StringList(vec!["second".into()])),
            ),
        ])));
        let payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> =
            Box::new(plain.clone()).into();
        let marked = payload.expose_plaintext();
        assert_eq!(marked, &plain);
        let Value::Concrete(ConcreteValue::Map(marked_map)) = marked else {
            panic!("map")
        };
        let Value::Concrete(ConcreteValue::Map(plain_map)) = &plain else {
            panic!("map")
        };
        let hash_string = |value: &Value| {
            let Value::Concrete(ConcreteValue::String(text)) = value else {
                panic!("string")
            };
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(
            hash_string(&marked_map["text"]),
            hash_string(&plain_map["text"])
        );
        let material = crate::value::canonical_secret_hash_material(&plain).unwrap();
        assert_eq!(material, br#"{"list":["second"],"text":"private-runtime"}"#);
        assert_eq!(
            crate::value::canonical_secret_hash_material(marked).unwrap(),
            material
        );
        let digest = payload.hash(None).unwrap();
        assert!(digest.verify(&material));
        assert_eq!(payload.hash(Some(&digest)).unwrap(), digest);
        assert_eq!(
            crate::value::value_to_json(marked).unwrap(),
            crate::value::value_to_json(&plain).unwrap()
        );
        assert_eq!(
            crate::protocol_convert::try_core_to_protocol_value(
                &(marked)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                    .into_known()
                    .unwrap()
            )
            .unwrap(),
            crate::protocol_convert::try_core_to_protocol_value(
                &plain
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                    .into_known()
                    .unwrap()
            )
            .unwrap()
        );
    }

    #[test]
    fn review3_payload_marks_all_string_leaves_at_construction() {
        use crate::resource::InterpolationPart;
        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("private-leaf".into())),
            Value::Concrete(ConcreteValue::StringList(vec!["private-list".into()])),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "key".into(),
                Value::Deferred(DeferredValue::Interpolation(vec![
                    InterpolationPart::Literal("private-literal".into()),
                    InterpolationPart::Expr(Value::Concrete(ConcreteValue::String(
                        "private-expr".into(),
                    ))),
                ])),
            )]))),
        ]));
        let payload: SecretPayload<NoPlaceholder, AbsoluteBindingAddress> =
            Box::new(value.clone()).into();
        let exposed = payload.expose_plaintext();
        assert_eq!(exposed, &value);
        assert!(!format!("{exposed:?}").contains("private-"));
        assert!(exposed.contains_diagnostic_sensitive_string());
    }

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
        payload.visit_plaintext_reference_inventory(|_, scope| {
            assert_eq!(scope, DiagnosticScope::Secret)
        });
        let error = payload
            .try_inspect(|value| Err::<(), _>(format!("{value:?} private-error")))
            .unwrap_err();
        assert_eq!(error, crate::resource::INSIDE_SECRET_MESSAGE);
        assert!(payload.visit(|value| matches!(value, Value::Concrete(ConcreteValue::String(_)))));
        assert_eq!(
            payload.try_inspect(|_| Ok::<_, String>(42_i64)).unwrap(),
            42
        );
    }
}
