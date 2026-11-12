//! Owning admission for the exact final provider payload.
//!
//! Reading a payload never releases a detachable receipt or mutable access.
//! Rewriting a cloned payload requires admission again.
//!
//! ```
//! use carina_core::validation::admission::final_input::FinalInput;
//! fn inspect<T>(input: &FinalInput<T>) -> &T { input.payload() }
//! ```
//! ```compile_fail,E0594
//! use carina_core::validation::admission::final_input::FinalInput;
//! fn replace<T>(input: &mut FinalInput<T>, other: T) { *input.payload() = other; }
//! ```
//! ```compile_fail,E0451
//! use carina_core::validation::admission::final_input::FinalInput;
//! fn rebind<T>(input: FinalInput<T>, other: T) -> FinalInput<T> {
//!     FinalInput { payload: other, ..input }
//! }
//! ```

use indexmap::IndexMap;

use crate::binding_index::ResolvedBindings;
use crate::executor::provider_ready::DispatchPayload;
use crate::provider::{ResourceWirePayload, TAGS_ATTRIBUTE, WireReady};
use crate::resource::knowledge::{Classification, KnowledgeContext, KnownValue, PlanResolved};
use crate::resource::{
    DefaultTagKeys, InsideSecret, ResolvedDataSource, ResolvedResource, ResourceId, Value,
};
use crate::schema::{FieldAncestry, LeafPath, LeafPathSegment, SchemaRegistry, TypeError};
use crate::validation::deferred_types::ProviderTypeCheckContext;

#[derive(Debug, Clone, PartialEq)]
pub struct FinalInput<T> {
    payload: T,
}

impl<T> FinalInput<T> {
    pub fn payload(&self) -> &T {
        &self.payload
    }
}

#[derive(Debug, Clone)]
pub enum FinalInputOrigin {
    ProviderNormalization,
    ObservedUpdatePatch,
    ProviderDefaultTags {
        keys: Vec<String>,
        instance: Option<String>,
    },
}

impl std::fmt::Display for FinalInputOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderNormalization => f.write_str("provider normalization"),
            Self::ObservedUpdatePatch => f.write_str("observed update patch"),
            Self::ProviderDefaultTags { keys, instance } => {
                f.write_str("provider default_tags")?;
                if let Some(instance) = instance {
                    write!(
                        f,
                        " instance '{}'",
                        crate::provider_text::DiagnosticText::new(instance)
                    )?;
                }
                for key in keys {
                    write!(f, " '{}'", crate::provider_text::DiagnosticText::new(key))?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FinalInputError {
    #[error("{resource}: final provider input has no destination schema")]
    MissingSchema { resource: Box<ResourceId> },
    #[error("{resource}: {path} from {origin}")] // error-chain: caller-renders-sources
    Membership {
        resource: Box<ResourceId>,
        path: LeafPath,
        origin: FinalInputOrigin,
        #[source]
        error: Box<TypeError>,
    },
    #[error(transparent)]
    Findings(FinalInputErrors),
}

/// A terminal collection retains every typed membership failure. Each finding
/// retains its source; rendering the collection includes each complete chain once.
#[derive(Debug)]
pub struct FinalInputErrors(Vec<FinalInputError>);

impl FinalInputErrors {
    pub fn findings(&self) -> &[FinalInputError] {
        &self.0
    }
}

impl std::fmt::Display for FinalInputErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, finding) in self.0.iter().enumerate() {
            if index != 0 {
                writeln!(f)?;
            }
            write!(f, "{finding}")?;
            let mut source = std::error::Error::source(finding);
            while let Some(error) = source {
                write!(f, ": {error}")?;
                source = error.source();
            }
        }
        Ok(())
    }
}
impl std::error::Error for FinalInputErrors {}

/// A single finding keeps its established source chain. Collections expose all
/// findings through the same accessor, without selecting a representative leaf.
impl FinalInputError {
    fn into_findings(self) -> Vec<Self> {
        match self {
            Self::Findings(errors) => errors.0,
            error => vec![error],
        }
    }

    pub fn findings(&self) -> &[Self] {
        match self {
            Self::Findings(errors) => errors.findings(),
            error => std::slice::from_ref(error),
        }
    }
}

enum Stage<'a> {
    Plan(&'a ResolvedBindings),
    Apply(&'a ResolvedBindings, FinalInputOrigin),
}

impl Stage<'_> {
    fn origin(&self) -> FinalInputOrigin {
        match self {
            Self::Plan(_) => FinalInputOrigin::ProviderNormalization,
            Self::Apply(_, origin) => origin.clone(),
        }
    }
}

struct FailureContext<'a> {
    id: &'a ResourceId,
    name: &'a str,
    protected: bool,
    origin: FinalInputOrigin,
    default_keys: Option<&'a DefaultTagKeys>,
    merged_default_keys: Vec<String>,
}

impl<'a> FailureContext<'a> {
    fn new(
        id: &'a ResourceId,
        name: &'a str,
        value: Option<&Value>,
        diagnostic: Option<&Value>,
        defaults: Option<&'a DefaultTagKeys>,
        stage: &Stage<'_>,
    ) -> Self {
        let protected = diagnostic.is_some_and(crate::value::contains_secret);
        let default_keys = defaults.filter(|_| name == TAGS_ATTRIBUTE);
        let merged_default_keys = match value {
            Some(Value::Concrete(crate::resource::ConcreteValue::Map(tags))) if !protected => tags
                .keys()
                .filter(|key| default_keys.is_some_and(|keys| keys.contains(key)))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        Self {
            id,
            name,
            protected,
            origin: stage.origin(),
            default_keys,
            merged_default_keys,
        }
    }

    fn finding(&self, path: LeafPath, error: TypeError) -> FinalInputError {
        let failing_key = match path.segments().first() {
            Some(LeafPathSegment::MapKey(key)) => Some(key),
            _ => None,
        };
        let from_defaults = self.default_keys.is_some_and(|keys| {
            failing_key.map_or_else(
                || !keys.is_empty(),
                |key| keys.contains(key.expose_plaintext()),
            )
        });
        let origin = if from_defaults {
            let keys = if failing_key.is_some_and(|key| key.is_diagnostic_sensitive()) {
                Vec::new()
            } else {
                self.merged_default_keys
                    .iter()
                    .filter(|key| failing_key.is_none_or(|failed| failed == *key))
                    .cloned()
                    .collect()
            };
            FinalInputOrigin::ProviderDefaultTags {
                keys,
                instance: self.id.provider_instance.clone(),
            }
        } else {
            self.origin.clone()
        };
        let path = if self.protected {
            path.inside_secret()
        } else {
            path
        };
        FinalInputError::Membership {
            resource: Box::new(self.id.clone()),
            path: LeafPath::from_segments(
                std::iter::once(LeafPathSegment::StructField(self.name.into()))
                    .chain(path.segments().iter().cloned()),
            ),
            origin,
            error: Box::new(if self.protected {
                error.inside_secret()
            } else {
                error
            }),
        }
    }
}

/// Knowledge selects the stage; the existing ValueValidator/value_walk owns
/// every structural, local, Union and provider semantic decision.
fn check(
    id: &ResourceId,
    attributes: &IndexMap<String, Value>,
    diagnostic_attributes: &IndexMap<String, Value>,
    defaults: Option<&DefaultTagKeys>,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    stage: Stage<'_>,
    session: &dyn ProviderTypeCheckContext,
) -> Result<(), FinalInputError> {
    let lookup = crate::parser::custom_type_lookup_for_owner(
        session.catalog().custom_type_resolver(),
        &id.provider,
    );
    let mut errors = Vec::new();
    for attribute in schema.schema().effective_attributes() {
        if attribute.is_required() && !attributes.contains_key(attribute.name()) {
            errors.push(
                FailureContext::new(id, attribute.name(), None, None, defaults, &stage).finding(
                    LeafPath::from_segments([]),
                    TypeError::MissingRequired {
                        name: attribute.name().into(),
                    },
                ),
            );
        }
    }
    for (name, value) in attributes {
        let failure = FailureContext::new(
            id,
            name,
            Some(value),
            diagnostic_attributes.get(name),
            defaults,
            &stage,
        );
        let Some(attribute) = schema.effective_attribute(name) else {
            errors.push(failure.finding(
                LeafPath::from_segments([]),
                TypeError::UnknownAttribute {
                    name: name.clone(),
                    suggestion: None,
                },
            ));
            continue;
        };
        let result = (|| {
            Ok(match &stage {
                Stage::Plan(bindings) => {
                    // Retain structural references in saved plans. Only the admission
                    // view resolves them; dispatch later admits the known payload.
                    let resolved = crate::resolver::resolve_ref_value(value, bindings, session)
                        .map_err(|error| TypeError::Reference {
                            error: Box::new(crate::resource::SecretDiagnosticError::Public(
                                super::AdmissionReferenceError::Resolution(Box::new(error)),
                            )),
                            attribute: None,
                        })?;
                    let proof = resolved
                        .knowledge(&KnowledgeContext::plan(bindings))
                        .and_then(PlanResolved::try_from)
                        .map_err(TypeError::from)?;
                    crate::schema::value_validation::validate_final_provider_value(
                        attribute.field_type(),
                        proof.knowledge(),
                        schema.schema().as_schema().defs(),
                        FieldAncestry::from_input_mode(attribute.effective_input_mode()),
                        &lookup,
                    )
                }
                Stage::Apply(bindings, _) => {
                    let proof = value
                        .knowledge(&KnowledgeContext::apply(bindings))
                        .and_then(KnownValue::from_apply)
                        .map_err(TypeError::from)?;
                    crate::schema::value_validation::validate_final_provider_value(
                        attribute.field_type(),
                        &Classification::<crate::resource::NoPlaceholder>::Known(proof),
                        schema.schema().as_schema().defs(),
                        FieldAncestry::from_input_mode(attribute.effective_input_mode()),
                        &lookup,
                    )
                }
            })
        })();
        match result {
            Ok(findings) => errors.extend(
                findings
                    .into_located()
                    .into_iter()
                    .map(|(path, error)| failure.finding(path, error)),
            ),
            Err(error) => errors.push(failure.finding(LeafPath::from_segments([]), error)),
        }
    }
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.pop().expect("one finding")),
        _ => Err(FinalInputError::Findings(FinalInputErrors(errors))),
    }
}

/// There is no constructor accepting an old receipt and a new payload.
pub(crate) fn admit_dispatch<T: ResourceWirePayload>(
    input: DispatchPayload<T>,
    defaults: Option<&DefaultTagKeys>,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    bindings: &ResolvedBindings,
    session: &dyn ProviderTypeCheckContext,
) -> Result<FinalInput<DispatchPayload<WireReady<T>>>, crate::executor::ProviderPreparationError> {
    // Preserve transport-error precedence with a single cached projection,
    // then admit the exact payload retained by that projection.
    let input = input.into_wire()?;
    check(
        input.payload().as_inner().resource_id(),
        input.payload().as_inner().attributes(),
        input.diagnostic_attributes(),
        defaults,
        schema,
        Stage::Apply(bindings, FinalInputOrigin::ProviderNormalization),
        session,
    )?;
    Ok(FinalInput { payload: input })
}

/// Patch construction may graft ignored observed leaves after resource
/// admission. Validate that exact result, then retain the patch immutably.
pub(crate) fn admit_update_patch(
    patch: crate::provider::UpdatePatch,
    resource: &crate::provider::ProviderReadyResource,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    bindings: &ResolvedBindings,
    session: &dyn ProviderTypeCheckContext,
) -> Result<
    FinalInput<WireReady<crate::provider::UpdatePatch>>,
    crate::executor::ProviderPreparationError,
> {
    let patch = WireReady::try_new(
        patch,
        crate::provider::ProviderWireTarget::Resource(resource.id.clone()),
    )?;
    let mut attributes = resource.attributes.clone();
    let mut diagnostic_attributes = attributes
        .iter()
        .map(|(name, value)| (name.clone(), resource.diagnostic_value(name, value)))
        .collect::<IndexMap<_, _>>();
    for op in &patch.as_inner().ops {
        if let Some(value) = op.sent_value() {
            attributes.insert(op.key().into(), value.clone());
            diagnostic_attributes
                .insert(op.key().into(), resource.diagnostic_value(op.key(), value));
        } else {
            attributes.shift_remove(op.key());
            diagnostic_attributes.shift_remove(op.key());
        }
    }
    check(
        &resource.id,
        &attributes,
        &diagnostic_attributes,
        Some(&resource.default_tag_keys),
        schema,
        Stage::Apply(bindings, FinalInputOrigin::ObservedUpdatePatch),
        session,
    )?;
    Ok(FinalInput { payload: patch })
}

/// Final plan inputs retain deferred apply values, but never dispatch authority.
/// Their immutable resources are the ones used to derive differ projections.
#[derive(Debug)]
pub struct FinalPlanInputs<S = crate::resource::ModuleSource> {
    resources: FinalInput<Vec<ResolvedResource<S>>>,
    data_sources: FinalInput<Vec<ResolvedDataSource<S>>>,
}

impl<S> FinalPlanInputs<S> {
    pub fn admit(
        resources: Vec<ResolvedResource<S>>,
        data_sources: Vec<ResolvedDataSource<S>>,
        schemas: &SchemaRegistry,
        bindings: &ResolvedBindings,
        session: &dyn ProviderTypeCheckContext,
    ) -> Result<Self, FinalInputError> {
        let mut errors = Vec::new();
        for resource in &resources {
            let raw = resource.as_resource();
            let result = match schemas.get_for(raw) {
                Some(schema) => check(
                    &raw.id,
                    &raw.attributes,
                    &raw.attributes,
                    Some(&raw.default_tag_keys),
                    schema,
                    Stage::Plan(bindings),
                    session,
                ),
                None => Err(FinalInputError::MissingSchema {
                    resource: Box::new(raw.id.clone()),
                }),
            };
            if let Err(error) = result {
                errors.extend(error.into_findings());
            }
        }
        for resource in &data_sources {
            let raw = resource.as_inner();
            let result = match schemas.get_for_data_source(raw) {
                Some(schema) => check(
                    &raw.id,
                    &raw.attributes,
                    &raw.attributes,
                    None,
                    schema,
                    Stage::Plan(bindings),
                    session,
                ),
                None => Err(FinalInputError::MissingSchema {
                    resource: Box::new(raw.id.clone()),
                }),
            };
            if let Err(error) = result {
                errors.extend(error.into_findings());
            }
        }
        match errors.len() {
            0 => {}
            1 => return Err(errors.pop().expect("one finding")),
            _ => return Err(FinalInputError::Findings(FinalInputErrors(errors))),
        }
        Ok(Self {
            resources: FinalInput { payload: resources },
            data_sources: FinalInput {
                payload: data_sources,
            },
        })
    }

    pub fn resources(&self) -> &[ResolvedResource<S>] {
        self.resources.payload()
    }

    pub fn into_resources(self) -> Vec<ResolvedResource<S>> {
        self.resources.payload
    }

    pub fn data_sources(&self) -> &[ResolvedDataSource<S>] {
        self.data_sources.payload()
    }
}
