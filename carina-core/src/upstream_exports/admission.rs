//! Admission of persisted exports to the source's consumer-visible contract.

use crate::value::{PersistedExportValue, PersistedValueDecodeError};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use super::{UpstreamExportEntries, UpstreamExportType};
use crate::resource::{Value, knowledge::KnowledgeContext};
use crate::validation::deferred_types::ProviderTypeCheckContext;
use crate::validation::typecheck::{TypeExprValueError, TypecheckProviderContext};

/// Loaded upstream snapshots. An absent binding remains unknown; an admitted
/// empty snapshot establishes that all its exports are absent.
pub type AdmittedRemoteBindings = HashMap<String, AdmittedUpstreamExports>;

/// A policy-classified export snapshot accepted by binding installation.
///
/// Known source types have passed admission. When the source has no known type,
/// values are explicitly installed without source-type admission; consumers still
/// check their actual values against destination types. This also covers explicit
/// named declarations unavailable in the consumer's installed schemas. Only
/// admission constructs this snapshot; it has no mutable view or deserialization
/// path.
///
/// ```compile_fail
/// use carina_core::upstream_exports::AdmittedUpstreamExports;
/// let exports = AdmittedUpstreamExports { values: Default::default() };
/// ```
///
/// ```compile_fail,E0308
/// use std::collections::HashMap;
/// use carina_core::{binding_index::IterableBindings, resource::Value};
/// fn install(raw: HashMap<String, HashMap<String, Value>>) {
///     let _ = IterableBindings::from_upstream_only(raw);
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedUpstreamExports {
    values: Arc<HashMap<String, Value>>,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamExportAdmissionError {
    #[error("upstream export '{binding}.{key}': {error}")]
    InvalidValue {
        binding: String,
        key: String,
        #[source]
        error: Box<TypeExprValueError>,
    },
    #[error("upstream export '{binding}.{key}': {error}")]
    UnresolvedValue {
        binding: String,
        key: String,
        #[source]
        error: crate::resource::knowledge::KnowledgeError,
    },
    #[error("upstream export '{binding}.{key}': {error}")]
    Decode {
        binding: String,
        key: String,
        #[source]
        error: PersistedValueDecodeError,
    },
}

impl AdmittedUpstreamExports {
    /// An unavailable source surface intentionally installs all supplied keys
    /// without source-type checking. Consumers still check the actual values.
    pub fn admit_snapshot(
        binding: &str,
        declarations: Option<&UpstreamExportEntries>,
        values: HashMap<String, Value>,
        context: &dyn ProviderTypeCheckContext,
    ) -> Result<Self, UpstreamExportAdmissionError> {
        Self::admit_supplied(
            binding,
            declarations,
            values,
            |value| Ok(Some(value)),
            context,
        )
    }

    /// Select source keys before decoding persisted values. Stale keys are never
    /// decoded; top-level null stays absent; installed nested nulls are errors.
    pub fn decode_and_admit<'a>(
        binding: &str,
        declarations: Option<&UpstreamExportEntries>,
        values: impl IntoIterator<Item = (String, &'a PersistedExportValue)>,
        context: &dyn ProviderTypeCheckContext,
    ) -> Result<Self, UpstreamExportAdmissionError> {
        Self::admit_supplied(
            binding,
            declarations,
            values,
            PersistedExportValue::decode,
            context,
        )
    }

    fn admit_supplied<S>(
        binding: &str,
        declarations: Option<&UpstreamExportEntries>,
        values: impl IntoIterator<Item = (String, S)>,
        decode: impl Fn(S) -> Result<Option<Value>, PersistedValueDecodeError>,
        context: &dyn ProviderTypeCheckContext,
    ) -> Result<Self, UpstreamExportAdmissionError> {
        let mut admitted = HashMap::new();
        for (key, supplied) in values {
            let declaration = match declarations {
                Some(surface) => match surface.get(&key) {
                    Some(declaration) => Some(declaration),
                    None => continue,
                },
                None => None,
            };
            let Some(value) =
                decode(supplied).map_err(|error| UpstreamExportAdmissionError::Decode {
                    binding: binding.into(),
                    key: key.clone(),
                    error,
                })?
            else {
                continue;
            };
            let known = value
                .knowledge(&KnowledgeContext::validation())
                .and_then(|proof| proof.into_classification().into_known())
                .map_err(|error| UpstreamExportAdmissionError::UnresolvedValue {
                    binding: binding.into(),
                    key: key.clone(),
                    error,
                })?;
            // None covers both unavailable declarations and an explicit
            // Unchecked declaration/inference result: InstalledWithoutSourceType.
            let expected = declaration.and_then(|entry| match &entry.static_type {
                UpstreamExportType::Static(static_type) => static_type.known(),
                UpstreamExportType::InferenceNotRun => None,
            });
            if let Some(expected) = expected {
                TypecheckProviderContext::for_session(context)
                    .check_upstream_export_constraints(expected, &known)
                    .require_provider_schemas()
                    .map_err(|error| UpstreamExportAdmissionError::InvalidValue {
                        binding: binding.into(),
                        key: key.clone(),
                        error: Box::new(error),
                    })?;
            }
            admitted.insert(key, value);
        }
        Ok(Self {
            values: Arc::new(admitted),
        })
    }
}

impl Deref for AdmittedUpstreamExports {
    type Target = HashMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

/// Install hand-built resolver fixtures without source-type checking.
/// This adapter does not read source directories or provider metadata.
#[cfg(any(test, feature = "test-utils"))]
pub fn remote_bindings_for_test(
    bindings: HashMap<String, HashMap<String, Value>>,
) -> AdmittedRemoteBindings {
    use crate::parser::ProviderContext;

    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    bindings
        .into_iter()
        .map(|(name, values)| {
            let admitted =
                crate::validation::deferred_types::with_session_for_test(&context, |session| {
                    AdmittedUpstreamExports::admit_snapshot(&name, None, values, session)
                })
                .expect("remote fixture values are fully resolved");
            (name, admitted)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, ResourceTypePath, TypeExpr};
    use crate::resource::ConcreteValue;

    #[test]
    fn review_5_unresolved_export_preserves_knowledge_error_source() {
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let declarations = HashMap::from([(
            "broken".into(),
            super::super::UpstreamExportEntry {
                static_type: UpstreamExportType::Static(
                    crate::static_type::ConsumerExportStaticType::Declared(
                        crate::static_type::StaticType::Known(
                            crate::static_type::ConsumerResolvedType::new(TypeExpr::Int),
                        ),
                    ),
                ),
                value: None,
            },
        )]);
        let error = crate::validation::deferred_types::with_session_for_test(&context, |session| {
            AdmittedUpstreamExports::admit_snapshot(
                "up",
                Some(&declarations),
                HashMap::from([(
                    "broken".into(),
                    Value::root_resource_ref("waiting", "id", vec![]),
                )]),
                session,
            )
        })
        .unwrap_err();
        let cause = std::error::Error::source(&error)
            .expect("admission must retain the knowledge classification failure");
        assert!(
            cause
                .downcast_ref::<crate::resource::knowledge::KnowledgeError>()
                .is_some(),
            "{cause:?}"
        );
    }
    use crate::static_type::{
        ConsumerExportStaticType, ConsumerInferredUncheckedReason, ConsumerResolvedType,
        InferenceError, NotInferableReason, StaticType,
    };

    struct NoWarnings;

    impl crate::validation::deferred_types::ProviderTypeCheckSink for NoWarnings {
        fn record(
            &mut self,
            diagnostic: crate::validation::deferred_types::ProviderTypeCheckDiagnostic,
        ) {
            panic!("source admission emitted a diagnostic: {diagnostic:?}");
        }
    }

    fn install_number(static_type: UpstreamExportType) -> AdmittedUpstreamExports {
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let declarations = HashMap::from([(
            "number".into(),
            super::super::UpstreamExportEntry {
                static_type,
                value: None,
            },
        )]);
        let checks =
            crate::validation::deferred_types::TypeCheckSession::run(&context, |session| {
                AdmittedUpstreamExports::admit_snapshot(
                    "orgs",
                    Some(&declarations),
                    HashMap::from([("number".into(), Value::Concrete(ConcreteValue::Int(73)))]),
                    session,
                )
            });
        assert!(checks.diagnostics().is_empty());
        checks.publish(&mut NoWarnings).unwrap()
    }

    #[test]
    fn inference_not_run_installs_without_source_admission() {
        let installed = install_number(UpstreamExportType::InferenceNotRun);
        assert_eq!(installed["number"], Value::Concrete(ConcreteValue::Int(73)));
    }

    #[test]
    fn inferred_unchecked_exports_reach_consumers_unchanged_without_warning() {
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let expected = TypeExpr::DottedUnresolved(ResourceTypePath::new("unavailable", "Thing.Id"));
        let StaticType::Unchecked(unnameable) =
            crate::validation::resolve_consumer_static_type(&expected, &context)
        else {
            panic!("fixture named type must be unresolved");
        };
        let reasons = vec![
            ConsumerInferredUncheckedReason::InferenceFailed(InferenceError::UnknownType {
                reason: "fixture inference failure".into(),
            }),
            ConsumerInferredUncheckedReason::NotInferable(NotInferableReason::ValueDependent {
                reason: "runtime lookup".into(),
            }),
            ConsumerInferredUncheckedReason::NotInferable(NotInferableReason::MissingValue),
            ConsumerInferredUncheckedReason::NotInferable(
                NotInferableReason::NonInferableBinding {
                    binding: "producer".into(),
                },
            ),
            ConsumerInferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                binding: "producer".into(),
            }),
            ConsumerInferredUncheckedReason::NotInferable(
                NotInferableReason::PlaceholderTypeUnavailable {
                    placeholder: "pending".into(),
                },
            ),
            ConsumerInferredUncheckedReason::NotInferable(
                NotInferableReason::CustomTypeSchemaUnavailable {
                    identity: crate::schema::TypeIdentity::from_schema_type(
                        "unavailable",
                        "Thing",
                        "Id",
                    ),
                },
            ),
            ConsumerInferredUncheckedReason::UnnameableInConsumer(unnameable),
        ];
        for reason in reasons {
            let installed = install_number(UpstreamExportType::Static(
                ConsumerExportStaticType::Inferred(StaticType::Unchecked(reason)),
            ));
            assert_eq!(installed["number"], Value::Concrete(ConcreteValue::Int(73)));

            let known = installed["number"]
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification()
                .into_known()
                .unwrap();
            assert!(
                crate::validation::deferred_types::with_session_for_test(&context, |session| {
                    TypecheckProviderContext::for_session(session)
                        .check_resolved_constraints(&TypeExpr::String, &known)
                        .require_provider_schemas()
                })
                .is_err(),
                "destination admission must still check the actual Int value"
            );
        }
    }

    #[test]
    fn declared_and_inferred_known_exports_are_checked() {
        for static_type in [
            ConsumerExportStaticType::Declared(StaticType::Known(ConsumerResolvedType::new(
                TypeExpr::Int,
            ))),
            ConsumerExportStaticType::Inferred(StaticType::Known(ConsumerResolvedType::new(
                TypeExpr::Int,
            ))),
        ] {
            let installed = install_number(UpstreamExportType::Static(static_type));
            assert_eq!(installed["number"], Value::Concrete(ConcreteValue::Int(73)));
        }
    }

    #[test]
    fn named_export_without_provider_metadata_installs_without_source_admission() {
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let expected = TypeExpr::DottedUnresolved(ResourceTypePath::new("unavailable", "Thing.Id"));
        let declarations = HashMap::from([(
            "id".into(),
            super::super::UpstreamExportEntry {
                static_type: UpstreamExportType::Static(ConsumerExportStaticType::Declared(
                    crate::validation::resolve_consumer_static_type(&expected, &context),
                )),
                value: None,
            },
        )]);
        let checks =
            crate::validation::deferred_types::TypeCheckSession::run(&context, |session| {
                AdmittedUpstreamExports::admit_snapshot(
                    "orgs",
                    Some(&declarations),
                    HashMap::from([(
                        "id".into(),
                        Value::Concrete(ConcreteValue::String("id-123".into())),
                    )]),
                    session,
                )
            });
        assert!(checks.diagnostics().is_empty());
        let installed = checks.publish(&mut NoWarnings).unwrap();
        assert_eq!(
            installed["id"],
            Value::Concrete(ConcreteValue::String("id-123".into()))
        );
    }
}
