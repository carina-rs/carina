//! Admission of persisted exports to the source's consumer-visible contract.

use std::collections::HashMap;
use std::ops::Deref;

use super::{UpstreamExportEntries, UpstreamExportType};
use crate::resource::{Value, knowledge::KnowledgeContext};
use crate::static_type::{ConsumerExportStaticType, StaticType, StaticTypeResolutionError};
use crate::validation::deferred_types::ProviderTypeCheckContext;
use crate::validation::typecheck::{TypeExprValueError, TypecheckProviderContext};

/// Loaded upstream snapshots. An absent binding remains unknown; an admitted
/// empty snapshot establishes that all its exports are absent.
pub type AdmittedRemoteBindings = HashMap<String, AdmittedUpstreamExports>;

/// A policy-classified export snapshot accepted by binding installation.
///
/// Known source types have passed admission. When the source has no known type,
/// values are explicitly installed without source-type admission; consumers still
/// check their actual values against destination types. An unresolved explicit
/// named declaration remains an error. Only admission constructs this snapshot;
/// it has no mutable view or deserialization path.
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
    values: HashMap<String, Value>,
    source_admission: HashMap<String, ExportSourceAdmission>,
}

/// The source-side evidence attached to one installed export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSourceAdmission {
    /// The value satisfies its declared-Known or inferred-Known source type.
    Admitted,
    /// Source inference supplied no known type. Destination checks use the
    /// actual value at plan/apply, as they do for unchecked references.
    InstalledWithoutSourceType,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamExportAdmissionError {
    #[error("upstream export '{binding}.{key}': source declaration could not be resolved: {error}")]
    DeclaredTypeUnavailable {
        binding: String,
        key: String,
        error: Box<StaticTypeResolutionError>,
    },
    #[error("upstream export '{binding}.{key}': {error}")]
    InvalidValue {
        binding: String,
        key: String,
        error: Box<TypeExprValueError>,
    },
    #[error("upstream export '{binding}.{key}': value is not fully resolved")]
    UnresolvedValue { binding: String, key: String },
}

impl AdmittedUpstreamExports {
    /// Classify every supplied source export without evaluating source values.
    /// Known types are checked; unavailable inference intentionally leaves the
    /// value for destination admission. Explicit named declarations still need
    /// downstream provider metadata. Stale keys are omitted.
    pub fn admit(
        binding: &str,
        declarations: &UpstreamExportEntries,
        values: HashMap<String, Value>,
        context: &dyn ProviderTypeCheckContext,
    ) -> Result<Self, UpstreamExportAdmissionError> {
        let mut admitted = HashMap::new();
        let mut source_admission = HashMap::new();
        for (key, value) in values {
            let Some(declaration) = declarations.get(&key) else {
                continue;
            };
            let expected = match &declaration.static_type {
                // This provenance means an explicit annotation failed named-type
                // resolution, rather than that source inference found no type.
                UpstreamExportType::Static(ConsumerExportStaticType::Declared(
                    StaticType::Unchecked(reason),
                )) => {
                    return Err(UpstreamExportAdmissionError::DeclaredTypeUnavailable {
                        binding: binding.into(),
                        key,
                        error: Box::new(reason.resolution_error().clone()),
                    });
                }
                UpstreamExportType::Static(static_type) => static_type.known(),
                UpstreamExportType::InferenceNotRun => None,
            };
            let classification = if let Some(expected) = expected {
                let known = value
                    .knowledge(&KnowledgeContext::validation())
                    .ok()
                    .and_then(|proof| proof.into_classification().into_known().ok())
                    .ok_or_else(|| UpstreamExportAdmissionError::UnresolvedValue {
                        binding: binding.into(),
                        key: key.clone(),
                    })?;
                TypecheckProviderContext::for_session(context)
                    .check_upstream_export_constraints(expected, &known)
                    .require_provider_schemas()
                    .map_err(|error| UpstreamExportAdmissionError::InvalidValue {
                        binding: binding.into(),
                        key: key.clone(),
                        error: Box::new(error),
                    })?;
                ExportSourceAdmission::Admitted
            } else {
                ExportSourceAdmission::InstalledWithoutSourceType
            };
            source_admission.insert(key.clone(), classification);
            admitted.insert(key, value);
        }
        Ok(Self {
            values: admitted,
            source_admission,
        })
    }

    pub fn source_admission(&self, key: &str) -> Option<ExportSourceAdmission> {
        self.source_admission.get(key).copied()
    }
}

impl Deref for AdmittedUpstreamExports {
    type Target = HashMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

/// Admit hand-built resolver fixtures against synthetic source declarations.
/// This adapter does not read source directories or provider metadata.
#[cfg(any(test, feature = "test-utils"))]
pub fn remote_bindings_for_test(
    bindings: HashMap<String, HashMap<String, Value>>,
) -> AdmittedRemoteBindings {
    use crate::parser::{ProviderContext, TypeExpr};
    use crate::static_type::{ConsumerExportStaticType, ConsumerResolvedType, StaticType};
    use crate::validation::inference::{RuntimeTypeInput, infer_runtime_type};

    fn fixture_type(value: &Value, context: &ProviderContext) -> TypeExpr {
        match value {
            Value::Concrete(crate::resource::ConcreteValue::Map(fields)) => TypeExpr::Struct {
                fields: fields
                    .iter()
                    .map(|(key, value)| (key.clone(), fixture_type(value, context)))
                    .collect(),
            },
            Value::Concrete(crate::resource::ConcreteValue::List(items)) => {
                let members = items
                    .iter()
                    .map(|value| fixture_type(value, context))
                    .collect::<Vec<_>>();
                TypeExpr::List(Box::new(match members.len() {
                    0 => TypeExpr::String,
                    1 => members.into_iter().next().unwrap(),
                    _ => TypeExpr::Union(members),
                }))
            }
            _ => {
                let state = crate::resource::State::existing(
                    crate::resource::ResourceId::with_identity("test.fixture", "exports"),
                    HashMap::from([("value".into(), value.clone())]),
                );
                infer_runtime_type(
                    RuntimeTypeInput::state_attribute(&state, "value").unwrap(),
                    &Default::default(),
                    &crate::schema::SchemaRegistry::new(),
                    context,
                )
                .expect("remote fixture has a runtime type")
            }
        }
    }

    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    bindings
        .into_iter()
        .map(|(name, values)| {
            let declarations = values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        super::UpstreamExportEntry {
                            static_type: UpstreamExportType::Static(
                                ConsumerExportStaticType::Declared(StaticType::Known(
                                    ConsumerResolvedType::new(fixture_type(value, &context)),
                                )),
                            ),
                            value: None,
                        },
                    )
                })
                .collect();
            let admitted =
                crate::validation::deferred_types::with_session_for_test(&context, |session| {
                    AdmittedUpstreamExports::admit(&name, &declarations, values, session)
                })
                .expect("remote fixture passes its synthetic export declaration");
            (name, admitted)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, ResourceTypePath, TypeExpr};
    use crate::resource::ConcreteValue;
    use crate::static_type::{
        ConsumerExportStaticType, ConsumerInferredUncheckedReason, ConsumerResolvedType,
        InferenceError, NotInferableReason,
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
                AdmittedUpstreamExports::admit(
                    "orgs",
                    &declarations,
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
        assert_eq!(
            installed.source_admission("number"),
            Some(ExportSourceAdmission::InstalledWithoutSourceType)
        );
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
            assert_eq!(
                installed.source_admission("number"),
                Some(ExportSourceAdmission::InstalledWithoutSourceType)
            );

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
    fn declared_and_inferred_known_exports_record_source_admission() {
        for static_type in [
            ConsumerExportStaticType::Declared(StaticType::Known(ConsumerResolvedType::new(
                TypeExpr::Int,
            ))),
            ConsumerExportStaticType::Inferred(StaticType::Known(ConsumerResolvedType::new(
                TypeExpr::Int,
            ))),
        ] {
            let installed = install_number(UpstreamExportType::Static(static_type));
            assert_eq!(
                installed.source_admission("number"),
                Some(ExportSourceAdmission::Admitted)
            );
        }
    }

    #[test]
    fn named_export_without_provider_metadata_is_a_typed_error() {
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
        let error = crate::validation::deferred_types::with_session_for_test(&context, |session| {
            AdmittedUpstreamExports::admit(
                "orgs",
                &declarations,
                HashMap::from([(
                    "id".into(),
                    Value::Concrete(ConcreteValue::String("id-123".into())),
                )]),
                session,
            )
        })
        .unwrap_err();
        assert!(matches!(
            error,
            UpstreamExportAdmissionError::DeclaredTypeUnavailable { .. }
                | UpstreamExportAdmissionError::InvalidValue { .. }
        ));
        assert!(error.to_string().contains("orgs.id"));
    }
}
