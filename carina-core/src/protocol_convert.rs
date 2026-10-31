//! The single typed conversion seam between `carina-core` and the provider
//! protocol.
//!
//! Schema encoding accepts validation witnesses, never mutable drafts:
//!
//! ```compile_fail
//! use carina_core::protocol_convert::resource_schema_to_protocol;
//! use carina_core::schema::ResourceSchema;
//!
//! let draft = ResourceSchema::new("test.Widget");
//! let _ = resource_schema_to_protocol(&draft);
//! ```
//!
//! Provider-config encoding has the same admission requirement:
//!
//! ```compile_fail
//! use std::collections::BTreeMap;
//! use carina_core::protocol_convert::config_schema_to_protocol;
//! use carina_core::schema::AttributeType;
//!
//! let draft = BTreeMap::<String, AttributeType>::new();
//! let _ = config_schema_to_protocol(&draft);
//! ```

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;

use carina_provider_protocol::types as protocol;
use indexmap::IndexMap;

use crate::provider::{
    AppliedState, PartialReadDiagnostic, PatchOp, ReadOutcome, UpdateOutcome, UpdatePatch,
};
use crate::resource::{
    ConcreteValue, DataSource, DeferredValue, Directives, RedactedSecretHash, Resource, ResourceId,
    ResourceIdentity, ResourceIdentityError, ResourceIdentityState, State, Value,
};
use crate::schema::{
    AttributeSchema, AttributeSchemaParts, AttributeType, ConfigSchemaValidationError, FieldGroup,
    FieldGroupKind, InputMode, RawShape, ResourceSchema, ResourceSchemaParts,
    ResourceSchemaValidationError, ResourceValidation, SchemaKind, SchemaValidationIssueLocation,
    StructField, StructFieldParts, TypeIdentity, UniqueNameSpec, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

fn field_group_to_protocol(group: &FieldGroup) -> protocol::FieldGroup {
    protocol::FieldGroup {
        kind: match group.kind() {
            FieldGroupKind::ExactlyOne => protocol::FieldGroupKind::ExactlyOne,
            FieldGroupKind::AtLeastOne => protocol::FieldGroupKind::AtLeastOne,
            FieldGroupKind::AtMostOne => protocol::FieldGroupKind::AtMostOne,
        },
        fields: group.fields().to_vec(),
    }
}

fn protocol_to_field_group(group: &protocol::FieldGroup) -> Result<FieldGroup, SchemaDecodeError> {
    let kind = match group.kind {
        protocol::FieldGroupKind::ExactlyOne => FieldGroupKind::ExactlyOne,
        protocol::FieldGroupKind::AtLeastOne => FieldGroupKind::AtLeastOne,
        protocol::FieldGroupKind::AtMostOne => FieldGroupKind::AtMostOne,
    };
    FieldGroup::new(kind, group.fields.clone()).map_err(|error| SchemaDecodeError::InvalidSchema {
        detail: "field group is invalid".to_string(),
        resource_type: None,
        attribute_path: Vec::new(),
        schema_issue_location: None,
        source: Some(Box::new(error)),
    })
}

/// A core value or type cannot be represented by the provider protocol.
#[derive(Debug)]
pub enum ProtocolConvertError {
    NonFiniteFloat,
    DurationOutOfRange,
    InvalidRedactedSecretDigest,
    NonWireValue {
        variant: &'static str,
    },
    NonWireAttributeType {
        kind: &'static str,
    },
    ValidatorWithoutIdentity {
        kind: &'static str,
    },
    OpenEnumWithoutNamespace {
        identity: TypeIdentity,
    },
    OpenEnumWithAliases {
        identity: TypeIdentity,
    },
    EmptyCustomEnumNamespace {
        name: String,
    },
    ClosedEnumWithTransform {
        identity: TypeIdentity,
    },
    ClosedEnumUnsupportedBase {
        identity: TypeIdentity,
        base_kind: String,
    },
    LegacyCustomUnsupportedBase {
        identity: String,
        base_kind: &'static str,
    },
    LegacyCustomUnsupportedMetadata {
        identity: String,
        field: &'static str,
        base_kind: &'static str,
    },
    Context {
        context: String,
        source: Box<ProtocolConvertError>,
    },
}

impl fmt::Display for ProtocolConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteFloat => {
                formatter.write_str("non-finite float cannot cross the provider protocol")
            }
            Self::DurationOutOfRange => formatter
                .write_str("duration in seconds does not fit the provider protocol integer domain"),
            Self::InvalidRedactedSecretDigest => {
                formatter.write_str("persisted state contains an invalid redacted-secret digest")
            }
            Self::NonWireValue { variant } => write!(
                formatter,
                "core value variant `{variant}` cannot cross the ordinary provider protocol boundary"
            ),
            Self::NonWireAttributeType { kind } => write!(
                formatter,
                "core attribute type `{kind}` has no provider protocol representation"
            ),
            Self::ValidatorWithoutIdentity { kind } => write!(
                formatter,
                "validator callback on {kind} is missing its required TypeIdentity"
            ),
            Self::OpenEnumWithoutNamespace { identity } => write!(
                formatter,
                "open enum `{identity}` has no namespace for the protocol CustomEnum record"
            ),
            Self::OpenEnumWithAliases { identity } => write!(
                formatter,
                "open enum `{identity}` carries DSL aliases that CustomEnum cannot represent"
            ),
            Self::EmptyCustomEnumNamespace { name } => write!(
                formatter,
                "protocol CustomEnum `{name}` must have a non-empty namespace"
            ),
            Self::ClosedEnumWithTransform { identity } => write!(
                formatter,
                "closed enum `{identity}` carries a DSL transform that StringEnum cannot represent"
            ),
            Self::ClosedEnumUnsupportedBase {
                identity,
                base_kind,
            } => write!(
                formatter,
                "closed enum `{identity}` has {base_kind} base metadata that StringEnum cannot represent"
            ),
            Self::LegacyCustomUnsupportedBase {
                identity,
                base_kind,
            } => write!(
                formatter,
                "legacy Custom `{identity}` has {base_kind} base metadata that core cannot preserve with the enclosing identity"
            ),
            Self::LegacyCustomUnsupportedMetadata {
                identity,
                field,
                base_kind,
            } => write!(
                formatter,
                "legacy Custom `{identity}` carries `{field}` metadata that a {base_kind} base cannot represent"
            ),
            Self::Context {
                context,
                source: _source,
            } => formatter.write_str(context),
        }
    }
}

impl Error for ProtocolConvertError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context {
                context: _context,
                source,
            } => Some(source.as_ref()),
            Self::NonFiniteFloat => None,
            Self::DurationOutOfRange => None,
            Self::InvalidRedactedSecretDigest => None,
            Self::NonWireValue { variant: _variant } => None,
            Self::NonWireAttributeType { kind: _kind } => None,
            Self::ValidatorWithoutIdentity { kind: _kind } => None,
            Self::OpenEnumWithoutNamespace {
                identity: _identity,
            } => None,
            Self::OpenEnumWithAliases {
                identity: _identity,
            } => None,
            Self::EmptyCustomEnumNamespace { name: _name } => None,
            Self::ClosedEnumWithTransform {
                identity: _identity,
            } => None,
            Self::ClosedEnumUnsupportedBase {
                identity: _identity,
                base_kind: _base_kind,
            } => None,
            Self::LegacyCustomUnsupportedBase {
                identity: _identity,
                base_kind: _field,
            } => None,
            Self::LegacyCustomUnsupportedMetadata {
                identity: _identity,
                field: _field,
                base_kind: _base_kind,
            } => None,
        }
    }
}

impl ProtocolConvertError {
    fn context(self, context: impl Into<String>) -> Self {
        Self::Context {
            context: context.into(),
            source: Box::new(self),
        }
    }
}

/// Typed failure while decoding untrusted provider schema records.
#[derive(Debug)]
pub enum SchemaDecodeError {
    InvalidSchema {
        detail: String,
        resource_type: Option<String>,
        attribute_path: Vec<String>,
        schema_issue_location: Option<Box<SchemaValidationIssueLocation>>,
        source: Option<Box<dyn Error + Send + Sync>>,
    },
    DuplicateReferenceForm {
        identity: TypeIdentity,
    },
    InvalidProviderSchema(crate::schema::ProviderSchemaValidationError),
}

impl SchemaDecodeError {
    fn new(detail: impl Into<String>) -> Self {
        Self::InvalidSchema {
            detail: detail.into(),
            resource_type: None,
            attribute_path: Vec::new(),
            schema_issue_location: None,
            source: None,
        }
    }

    fn from_default(error: ProtocolConvertError) -> Self {
        Self::InvalidSchema {
            detail: "schema default value conversion error".to_string(),
            resource_type: None,
            attribute_path: Vec::new(),
            schema_issue_location: None,
            source: Some(Box::new(error)),
        }
    }

    fn config_schema_validation(error: ConfigSchemaValidationError) -> Self {
        Self::InvalidSchema {
            detail: "provider config schema is invalid".to_string(),
            resource_type: None,
            attribute_path: Vec::new(),
            schema_issue_location: None,
            source: Some(Box::new(error)),
        }
    }

    fn resource_schema_validation(
        resource_type: &str,
        error: ResourceSchemaValidationError,
    ) -> Self {
        let schema_issue_location = error
            .located_issues()
            .next()
            .map(|(location, _message)| location.clone());
        Self::InvalidSchema {
            detail: "resource schema is invalid".to_string(),
            resource_type: Some(resource_type.to_string()),
            attribute_path: Vec::new(),
            schema_issue_location: schema_issue_location.map(Box::new),
            source: Some(Box::new(error)),
        }
    }

    fn conflicting_input_mode() -> Self {
        Self::new("input mode cannot be both required and read-only")
    }

    fn conflicting_write_only_mode() -> Self {
        Self::new("struct field cannot be both write-only and provider-populated")
    }

    fn conflicting_attribute_write_only_mode() -> Self {
        Self::new("attribute cannot be both write-only and provider-populated")
    }

    fn incompatible_attribute_modifier(mode: InputMode, modifier: &str) -> Self {
        Self::new(format!(
            "{modifier} is not valid for {} attribute input mode",
            input_mode_name(mode)
        ))
    }

    fn empty_custom_enum_namespace(name: &str) -> Self {
        Self::InvalidSchema {
            detail: "custom enum namespace is invalid".to_string(),
            resource_type: None,
            attribute_path: Vec::new(),
            schema_issue_location: None,
            source: Some(Box::new(ProtocolConvertError::EmptyCustomEnumNamespace {
                name: name.to_string(),
            })),
        }
    }

    fn opaque_validator(kind: &str) -> Self {
        Self::new(format!(
            "non-null opaque validator payload on protocol {kind} cannot be reconstructed"
        ))
    }

    fn protocol(error: ProtocolConvertError, detail: &'static str) -> Self {
        Self::InvalidSchema {
            detail: detail.to_string(),
            resource_type: None,
            attribute_path: Vec::new(),
            schema_issue_location: None,
            source: Some(Box::new(error)),
        }
    }

    fn unsupported_custom_base(enclosing_custom_name: &str, base_kind: &'static str) -> Self {
        Self::protocol(
            ProtocolConvertError::LegacyCustomUnsupportedBase {
                identity: enclosing_custom_name.to_string(),
                base_kind,
            },
            "legacy Custom type cannot be represented without losing metadata",
        )
    }

    fn legacy_custom_int_bound_out_of_range(enclosing_custom_name: &str, value: u64) -> Self {
        Self::new(format!(
            "legacy Custom wire type '{enclosing_custom_name}' integer bound {value} exceeds i64"
        ))
    }

    fn prepend_attribute(mut self, name: &str) -> Self {
        match &mut self {
            Self::InvalidSchema {
                detail: _detail,
                resource_type: _resource_type,
                attribute_path,
                schema_issue_location: _schema_issue_location,
                source: _source,
            } => {
                attribute_path.insert(0, name.to_string());
            }
            Self::DuplicateReferenceForm { identity: _ } | Self::InvalidProviderSchema(_) => {}
        }
        self
    }

    fn with_resource_type(mut self, resource_type: &str) -> Self {
        match &mut self {
            Self::InvalidSchema {
                detail: _detail,
                resource_type: current,
                attribute_path: _attribute_path,
                schema_issue_location: _schema_issue_location,
                source: _source,
            } => *current = Some(resource_type.to_string()),
            Self::DuplicateReferenceForm { identity: _ } | Self::InvalidProviderSchema(_) => {}
        }
        self
    }
}

impl fmt::Display for SchemaDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema {
                detail,
                resource_type,
                attribute_path,
                schema_issue_location,
                source: _source,
            } => {
                if let Some(resource_type) = resource_type {
                    write!(formatter, "resource '{resource_type}'")?;
                    if !attribute_path.is_empty() {
                        write!(formatter, " attribute '{}'", attribute_path.join("."))?;
                    } else if let Some(attribute_path) = schema_issue_location
                        .as_deref()
                        .and_then(SchemaValidationIssueLocation::attribute_path)
                    {
                        write!(formatter, " attribute '{attribute_path}'")?;
                    }
                    formatter.write_str(": ")?;
                    if let Some(location) = schema_issue_location
                        && location.attribute_path().is_none()
                    {
                        write!(formatter, "{location}: ")?;
                    }
                    formatter.write_str(detail)?;
                } else if attribute_path.is_empty() {
                    formatter.write_str(detail)?;
                } else {
                    write!(
                        formatter,
                        "attribute '{}': {}",
                        attribute_path.join("."),
                        detail
                    )?;
                }
            }
            Self::DuplicateReferenceForm { identity } => {
                write!(formatter, "duplicate reference form identity `{identity}`")?;
            }
            Self::InvalidProviderSchema(error) => error.fmt(formatter)?,
        }
        Ok(())
    }
}

impl Error for SchemaDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSchema {
                detail: _detail,
                resource_type: _resource_type,
                attribute_path: _attribute_path,
                schema_issue_location: _schema_issue_location,
                source,
            } => source
                .as_deref()
                .map(|source| source as &(dyn Error + 'static)),
            Self::DuplicateReferenceForm { identity: _ } => None,
            Self::InvalidProviderSchema(error) => Some(error),
        }
    }
}

/// Convert a validated resource schema into its version-13 protocol record.
pub fn resource_schema_to_protocol(
    schema: &ValidatedResourceSchema,
) -> Result<protocol::ResourceSchema, ProtocolConvertError> {
    let ResourceSchemaParts {
        resource_type,
        attributes,
        description,
        validation,
        kind,
        unique_name,
        operation_config,
        field_groups,
        default_wait_timeout,
        default_wait_interval,
        defs,
    } = schema.parts();
    // Protocol v13 has no resource wait-default fields. These are host-side
    // execution policy, not provider metadata; projection intentionally omits
    // them and round-trip tests pin that one-way behavior.
    let _v13_omits_host_wait_defaults = (default_wait_timeout, default_wait_interval);

    let attributes = attributes
        .iter()
        .map(|(name, attribute)| {
            attribute_schema_to_protocol(attribute)
                .map(|converted| (name.clone(), converted))
                .map_err(|error| error.context(format!("attribute `{name}`")))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let defs = defs
        .iter()
        .map(|(name, definition)| {
            attribute_type_to_protocol(definition)
                .map(|converted| (name.clone(), converted))
                .map_err(|error| error.context(format!("definition `{name}`")))
        })
        .collect::<Result<_, _>>()?;
    let validators = match validation {
        ResourceValidation::None => Vec::new(),
        ResourceValidation::Declared(validators) => validators.clone(),
    };
    let kind = match kind {
        SchemaKind::Resource => protocol::SchemaKind::Managed,
        SchemaKind::DataSource => protocol::SchemaKind::DataSource,
    };
    let unique_name = match unique_name {
        UniqueNameSpec::Attribute(attribute) => {
            protocol::UniqueNameSpec::Attribute(attribute.clone())
        }
        UniqueNameSpec::Coexisting => protocol::UniqueNameSpec::Coexisting,
        UniqueNameSpec::Conflicting => protocol::UniqueNameSpec::Conflicting,
    };

    Ok(protocol::ResourceSchema {
        resource_type: resource_type.to_string(),
        attributes,
        description: description.map(str::to_owned),
        kind,
        unique_name,
        operation_config: operation_config.cloned(),
        validators,
        field_groups: field_groups.iter().map(field_group_to_protocol).collect(),
        defs,
    })
}

/// Convert several validated schemas for SDK metadata serialization.
pub fn resource_schemas_to_protocol<'a>(
    schemas: impl IntoIterator<Item = &'a ValidatedResourceSchema>,
) -> Result<Vec<protocol::ResourceSchema>, ProtocolConvertError> {
    schemas
        .into_iter()
        .map(resource_schema_to_protocol)
        .collect()
}

fn type_identity_to_protocol(identity: &TypeIdentity) -> String {
    identity.to_string()
}

fn protocol_to_type_identity(identity: &str) -> TypeIdentity {
    TypeIdentity::from_dotted(identity)
}

/// Project one aggregate provider-schema witness onto the version-13 envelope.
pub fn provider_schema_to_protocol(
    witness: &ValidatedProviderSchemaWitness,
) -> Result<protocol::ProviderSchemaEnvelope, ProtocolConvertError> {
    let resources = resource_schemas_to_protocol(witness.resources())?;
    let reference_forms = witness
        .reference_forms()
        .iter()
        .map(|(identity, form)| {
            let form = match form {
                protocol::ReferenceForm::Template { long } => {
                    protocol::ReferenceForm::Template { long: long.clone() }
                }
            };
            protocol::ReferenceFormDeclaration {
                identity: type_identity_to_protocol(identity),
                form,
            }
        })
        .collect();
    Ok(protocol::ProviderSchemaEnvelope {
        resources,
        reference_forms,
    })
}

/// Decode the version-13 provider-schema envelope and jointly admit resource,
/// config, and reference-form declarations.
pub fn protocol_to_provider_schema_witness(
    envelope: &protocol::ProviderSchemaEnvelope,
    config: ValidatedConfigSchema,
) -> Result<ValidatedProviderSchemaWitness, SchemaDecodeError> {
    let protocol::ProviderSchemaEnvelope {
        resources,
        reference_forms,
    } = envelope;
    let resources = protocol_to_resource_schemas(resources)?;
    let mut forms = crate::schema::ReferenceForms::new();
    for declaration in reference_forms {
        let protocol::ReferenceFormDeclaration { identity, form } = declaration;
        let identity = protocol_to_type_identity(identity);
        let form = match form {
            protocol::ReferenceForm::Template { long } => {
                protocol::ReferenceForm::Template { long: long.clone() }
            }
        };
        if forms.insert(identity.clone(), form).is_some() {
            return Err(SchemaDecodeError::DuplicateReferenceForm { identity });
        }
    }
    ValidatedProviderSchemaWitness::new(resources, config, forms)
        .map_err(SchemaDecodeError::InvalidProviderSchema)
}

/// Convert a validated provider-config schema to protocol attribute types.
pub fn config_schema_to_protocol(
    schema: &ValidatedConfigSchema,
) -> Result<BTreeMap<String, protocol::AttributeType>, ProtocolConvertError> {
    schema
        .parts()
        .iter()
        .map(|(name, attribute_type)| {
            attribute_type_to_protocol(attribute_type)
                .map(|converted| (name.clone(), converted))
                .map_err(|error| error.context(format!("config attribute `{name}`")))
        })
        .collect()
}

pub fn attribute_schema_to_protocol(
    attribute: &AttributeSchema,
) -> Result<protocol::AttributeSchema, ProtocolConvertError> {
    let AttributeSchemaParts {
        name,
        attr_type,
        declared_input_mode,
        default,
        description,
        completions,
        provider_name,
        create_only,
        removable,
        block_name,
        write_only,
        keyed_partial_update,
        identity,
        deferred_populate,
    } = attribute.parts();
    // Protocol v13 has no completion field. Completions are an in-process
    // authoring aid, so JSON metadata projection intentionally omits them.
    let _v13_omits_host_completions = completions;
    let (required, read_only) = input_mode_to_protocol(declared_input_mode);
    let default = default
        .map(try_core_to_protocol_value)
        .transpose()
        .map_err(|error| error.context(format!("default for `{name}`")))?;
    Ok(protocol::AttributeSchema {
        name: name.to_string(),
        attr_type: attribute_type_to_protocol(attr_type)?,
        required,
        keyed_partial_update: keyed_partial_update
            .map(|keyed| keyed.key_field().as_str().to_string()),
        default,
        description: description.map(str::to_owned),
        create_only,
        read_only,
        write_only,
        block_name: block_name.map(str::to_owned),
        provider_name: provider_name.map(str::to_owned),
        removable,
        identity,
        deferred_populate,
    })
}

/// Convert one core attribute type to its protocol representation.
pub fn attribute_type_to_protocol(
    attribute_type: &AttributeType,
) -> Result<protocol::AttributeType, ProtocolConvertError> {
    match attribute_type.raw_shape() {
        RawShape::String {
            identity,
            pattern,
            length,
            validate,
            to_dsl,
        } => {
            ensure_validator_identity("String", identity, validate)?;
            let _validate_via_identity = validate;
            Ok(protocol::AttributeType::String {
                pattern: pattern.map(str::to_owned),
                length,
                validate: None,
                to_dsl: to_dsl.cloned(),
                identity: identity.map(type_identity_to_protocol),
            })
        }
        RawShape::Int {
            identity,
            range,
            validate,
        } => {
            ensure_validator_identity("Int", identity, validate)?;
            let _validate_via_identity = validate;
            Ok(protocol::AttributeType::Int {
                range,
                identity: identity.map(type_identity_to_protocol),
            })
        }
        RawShape::Float {
            identity,
            range,
            validate,
        } => {
            ensure_validator_identity("Float", identity, validate)?;
            let _validate_via_identity = validate;
            Ok(protocol::AttributeType::Float {
                range,
                identity: identity.map(type_identity_to_protocol),
            })
        }
        RawShape::Bool => Ok(protocol::AttributeType::Bool),
        RawShape::Duration => Ok(protocol::AttributeType::Duration),
        RawShape::Enum {
            identity,
            base,
            values,
            dsl_aliases,
            validate,
            to_dsl,
        } => {
            let _validate_via_identity = validate;
            match values {
                Some(values) => {
                    if to_dsl.is_some() {
                        return Err(ProtocolConvertError::ClosedEnumWithTransform {
                            identity: identity.clone(),
                        });
                    }
                    if !matches!(
                        base.raw_shape(),
                        RawShape::String {
                            identity: None,
                            pattern: None,
                            length: None,
                            validate: None,
                            to_dsl: None,
                        }
                    ) {
                        return Err(ProtocolConvertError::ClosedEnumUnsupportedBase {
                            identity: identity.clone(),
                            base_kind: base.type_name(),
                        });
                    }
                    Ok(protocol::AttributeType::StringEnum {
                        values: values.to_vec(),
                        name: identity.kind.clone(),
                        namespace: identity.dotted_prefix(),
                        dsl_aliases: dsl_aliases.to_vec(),
                    })
                }
                None => {
                    if !dsl_aliases.is_empty() {
                        return Err(ProtocolConvertError::OpenEnumWithAliases {
                            identity: identity.clone(),
                        });
                    }
                    let namespace = identity.dotted_prefix().ok_or_else(|| {
                        ProtocolConvertError::OpenEnumWithoutNamespace {
                            identity: identity.clone(),
                        }
                    })?;
                    Ok(protocol::AttributeType::CustomEnum {
                        name: identity.kind.clone(),
                        base: Box::new(attribute_type_to_protocol(base)?),
                        namespace,
                        dsl_transform: to_dsl.cloned(),
                    })
                }
            }
        }
        RawShape::List {
            element_type,
            ordered,
            length,
        } => Ok(protocol::AttributeType::List {
            element_type: Box::new(attribute_type_to_protocol(element_type)?),
            ordered,
            length,
            validate: None,
        }),
        RawShape::Map { key, value } => Ok(protocol::AttributeType::Map {
            inner: Box::new(attribute_type_to_protocol(value)?),
            key: Box::new(attribute_type_to_protocol(key)?),
        }),
        RawShape::Struct {
            name,
            fields,
            field_groups,
        } => Ok(protocol::AttributeType::Struct {
            name: name.to_string(),
            fields: fields
                .iter()
                .map(struct_field_to_protocol)
                .collect::<Result<_, _>>()?,
            field_groups: field_groups.iter().map(field_group_to_protocol).collect(),
        }),
        RawShape::Union(members) => Ok(protocol::AttributeType::Union {
            members: members
                .iter()
                .map(attribute_type_to_protocol)
                .collect::<Result<_, _>>()?,
        }),
        RawShape::Ref(name) => Ok(protocol::AttributeType::Ref {
            name: name.to_string(),
        }),
    }
}

fn ensure_validator_identity(
    kind: &'static str,
    identity: Option<&TypeIdentity>,
    validate: Option<&crate::schema::CustomValidator>,
) -> Result<(), ProtocolConvertError> {
    if validate.is_some() && identity.is_none() {
        Err(ProtocolConvertError::ValidatorWithoutIdentity { kind })
    } else {
        Ok(())
    }
}

pub fn struct_field_to_protocol(
    field: &StructField,
) -> Result<protocol::StructField, ProtocolConvertError> {
    let StructFieldParts {
        name,
        field_type,
        declared_input_mode,
        description,
        provider_name,
        block_name,
        deferred_populate,
        write_only,
    } = field.parts();
    let (required, read_only) = input_mode_to_protocol(declared_input_mode);
    Ok(protocol::StructField {
        name: name.to_string(),
        field_type: attribute_type_to_protocol(field_type)?,
        required,
        description: description.map(str::to_owned),
        block_name: block_name.map(str::to_owned),
        provider_name: provider_name.map(str::to_owned),
        read_only,
        write_only,
        deferred_populate,
    })
}

pub fn input_mode_to_protocol(input_mode: InputMode) -> (bool, bool) {
    match input_mode {
        InputMode::Optional => (false, false),
        InputMode::Required => (true, false),
        InputMode::ProviderPopulated => (false, true),
    }
}

/// Decode a protocol resource schema and return a whole-schema witness.
pub fn protocol_to_resource_schema(
    schema: &protocol::ResourceSchema,
) -> Result<ValidatedResourceSchema, SchemaDecodeError> {
    let protocol::ResourceSchema {
        resource_type,
        attributes,
        description,
        kind,
        unique_name,
        operation_config,
        validators,
        field_groups,
        defs,
    } = schema;

    let mut core = ResourceSchema::new(resource_type.clone());
    if let Some(description) = description {
        core = core.with_description(description.clone());
    }
    if !validators.is_empty() {
        core = core.with_declared_validators(validators.clone());
    }
    core = match kind {
        protocol::SchemaKind::Managed => core,
        protocol::SchemaKind::DataSource => core.as_data_source(),
    };
    core = match unique_name {
        protocol::UniqueNameSpec::Attribute(attribute) => {
            core.with_unique_name_attribute(attribute.clone())
        }
        protocol::UniqueNameSpec::Coexisting => core.with_coexisting_replacement(),
        protocol::UniqueNameSpec::Conflicting => core.with_conflicting_replacement(),
    };
    if let Some(operation_config) = operation_config {
        core = core.with_operation_config(operation_config.clone());
    }
    for (index, group) in field_groups.iter().enumerate() {
        core = core.field_group(protocol_to_field_group(group).map_err(|error| {
            error
                .prepend_attribute(&format!("field_groups[{index}]"))
                .with_resource_type(resource_type)
        })?);
    }

    for (name, definition) in defs {
        let definition = protocol_to_attribute_type(definition).map_err(|error| {
            error
                .prepend_attribute(name)
                .with_resource_type(resource_type)
        })?;
        core = core.with_def(name.clone(), definition);
    }
    for (map_name, attribute) in attributes {
        let attribute = protocol_to_attribute_schema(attribute)
            .map_err(|error| error.with_resource_type(resource_type))?;
        if attribute.name() != map_name {
            return Err(SchemaDecodeError::new(
                "attribute map key does not match the attribute schema name",
            )
            .prepend_attribute(map_name)
            .with_resource_type(resource_type));
        }
        core = core.attribute(attribute);
    }
    core.validate()
        .map_err(|error| SchemaDecodeError::resource_schema_validation(resource_type, error))
}

/// Decode several untrusted protocol resource schema records.
pub fn protocol_to_resource_schemas(
    schemas: &[protocol::ResourceSchema],
) -> Result<Vec<ValidatedResourceSchema>, SchemaDecodeError> {
    schemas.iter().map(protocol_to_resource_schema).collect()
}

/// Decode protocol provider-config records and return a graph-validation witness.
pub fn protocol_to_config_schema(
    attribute_types: BTreeMap<String, protocol::AttributeType>,
) -> Result<ValidatedConfigSchema, SchemaDecodeError> {
    let attribute_types = attribute_types
        .into_iter()
        .map(|(name, attribute_type)| {
            protocol_to_attribute_type(&attribute_type)
                .map(|converted| (name.clone(), converted))
                .map_err(|error| error.prepend_attribute(&name))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    ValidatedConfigSchema::validate(attribute_types)
        .map_err(SchemaDecodeError::config_schema_validation)
}

pub fn protocol_to_attribute_schema(
    attribute: &protocol::AttributeSchema,
) -> Result<AttributeSchema, SchemaDecodeError> {
    let protocol::AttributeSchema {
        name,
        attr_type,
        required,
        keyed_partial_update,
        default,
        description,
        create_only,
        read_only,
        write_only,
        block_name,
        provider_name,
        removable,
        identity,
        deferred_populate,
    } = attribute;

    if *read_only && *write_only {
        return Err(
            SchemaDecodeError::conflicting_attribute_write_only_mode().prepend_attribute(name)
        );
    }
    let input_mode = protocol_to_input_mode(*required, *read_only)
        .map_err(|error| error.prepend_attribute(name))?;
    let attr_type =
        protocol_to_attribute_type(attr_type).map_err(|error| error.prepend_attribute(name))?;
    let default = default
        .as_ref()
        .map(protocol_schema_default_to_core)
        .transpose()
        .map_err(SchemaDecodeError::from_default)
        .map_err(|error| error.prepend_attribute(name))?;

    match input_mode {
        InputMode::Optional => {
            if *deferred_populate {
                return Err(SchemaDecodeError::incompatible_attribute_modifier(
                    input_mode,
                    "deferred_populate",
                )
                .prepend_attribute(name));
            }
            let mut builder = AttributeSchema::optional(name.clone(), attr_type);
            if let Some(default) = default {
                builder = builder.with_default(default);
            }
            if let Some(description) = description {
                builder = builder.with_description(description.clone());
            }
            if let Some(provider_name) = provider_name {
                builder = builder.with_provider_name(provider_name.clone());
            }
            if *create_only {
                builder = builder.create_only();
            }
            builder = match removable {
                Some(true) => builder.removable(),
                Some(false) => builder.non_removable(),
                None => builder,
            };
            if let Some(block_name) = block_name {
                builder = builder.with_block_name(block_name.clone());
            }
            if *write_only {
                builder = builder.write_only();
            }
            if *identity {
                builder = builder.identity();
            }
            if let Some(key_field) = keyed_partial_update {
                builder = builder.keyed_partial_update(key_field.clone());
            }
            Ok(builder.build())
        }
        InputMode::Required => {
            if default.is_some() {
                return Err(SchemaDecodeError::incompatible_attribute_modifier(
                    input_mode, "default",
                )
                .prepend_attribute(name));
            }
            if removable.is_some() {
                return Err(SchemaDecodeError::incompatible_attribute_modifier(
                    input_mode,
                    "removable",
                )
                .prepend_attribute(name));
            }
            if *deferred_populate {
                return Err(SchemaDecodeError::incompatible_attribute_modifier(
                    input_mode,
                    "deferred_populate",
                )
                .prepend_attribute(name));
            }
            let mut builder = AttributeSchema::required(name.clone(), attr_type);
            if let Some(description) = description {
                builder = builder.with_description(description.clone());
            }
            if let Some(provider_name) = provider_name {
                builder = builder.with_provider_name(provider_name.clone());
            }
            if *create_only {
                builder = builder.create_only();
            }
            if let Some(block_name) = block_name {
                builder = builder.with_block_name(block_name.clone());
            }
            if *write_only {
                builder = builder.write_only();
            }
            if *identity {
                builder = builder.identity();
            }
            if let Some(key_field) = keyed_partial_update {
                builder = builder.keyed_partial_update(key_field.clone());
            }
            Ok(builder.build())
        }
        InputMode::ProviderPopulated => {
            for (present, modifier) in [
                (default.is_some(), "default"),
                (*create_only, "create_only"),
                (*write_only, "write_only"),
                (block_name.is_some(), "block_name"),
                (removable.is_some(), "removable"),
                (*identity, "identity"),
                (keyed_partial_update.is_some(), "keyed_partial_update"),
            ] {
                if present {
                    return Err(SchemaDecodeError::incompatible_attribute_modifier(
                        input_mode, modifier,
                    )
                    .prepend_attribute(name));
                }
            }
            let mut builder = AttributeSchema::provider_populated(name.clone(), attr_type);
            if let Some(description) = description {
                builder = builder.with_description(description.clone());
            }
            if let Some(provider_name) = provider_name {
                builder = builder.with_provider_name(provider_name.clone());
            }
            if *deferred_populate {
                builder = builder.deferred_populate();
            }
            Ok(builder.build())
        }
    }
}

/// Decode one protocol attribute type, rebuilding core-owned validators by identity.
pub fn protocol_to_attribute_type(
    attribute_type: &protocol::AttributeType,
) -> Result<AttributeType, SchemaDecodeError> {
    match attribute_type {
        protocol::AttributeType::String {
            pattern,
            length,
            validate,
            to_dsl,
            identity,
        } => {
            reject_opaque_validator("String", validate)?;
            Ok(decoded_refined_string(
                decode_optional_identity(identity),
                pattern.clone(),
                *length,
                to_dsl.clone(),
            ))
        }
        protocol::AttributeType::Int { range, identity } => Ok(decoded_refined_int(
            decode_optional_identity(identity),
            *range,
        )),
        protocol::AttributeType::Float { range, identity } => Ok(decoded_refined_float(
            decode_optional_identity(identity),
            *range,
        )),
        protocol::AttributeType::Bool => Ok(AttributeType::bool()),
        protocol::AttributeType::Duration => Ok(AttributeType::duration()),
        protocol::AttributeType::StringEnum {
            values,
            name,
            namespace,
            dsl_aliases,
        } => {
            let identity = namespace
                .as_deref()
                .filter(|namespace| !namespace.is_empty())
                .map(|namespace| crate::schema::enum_identity(name, Some(namespace)))
                .unwrap_or_else(|| TypeIdentity::bare(name));
            Ok(AttributeType::enum_(
                identity,
                Some(values.clone()),
                dsl_aliases.clone(),
                None,
                None,
            ))
        }
        protocol::AttributeType::List {
            element_type,
            ordered,
            length,
            validate,
        } => {
            reject_opaque_validator("List", validate)?;
            Ok(AttributeType::refined_list(
                protocol_to_attribute_type(element_type)?,
                *ordered,
                *length,
            ))
        }
        protocol::AttributeType::Map { inner, key } => Ok(AttributeType::map_with_key(
            protocol_to_attribute_type(key)?,
            protocol_to_attribute_type(inner)?,
        )),
        protocol::AttributeType::Struct {
            name,
            fields,
            field_groups,
        } => Ok(AttributeType::struct_with_field_groups(
            name.clone(),
            fields
                .iter()
                .map(protocol_to_struct_field)
                .collect::<Result<_, _>>()?,
            field_groups
                .iter()
                .enumerate()
                .map(|(index, group)| {
                    protocol_to_field_group(group)
                        .map_err(|error| error.prepend_attribute(&format!("field_groups[{index}]")))
                })
                .collect::<Result<_, _>>()?,
        )),
        protocol::AttributeType::Union { members } => Ok(AttributeType::union(
            members
                .iter()
                .map(protocol_to_attribute_type)
                .collect::<Result<_, _>>()?,
        )),
        protocol::AttributeType::Custom {
            name,
            base,
            pattern,
            length,
            validate,
            to_dsl,
        } => {
            reject_opaque_validator("Custom", validate)?;
            protocol_legacy_custom_to_core(name, base, pattern.clone(), *length, to_dsl.clone())
        }
        protocol::AttributeType::CustomEnum {
            name,
            base,
            namespace,
            dsl_transform,
        } => {
            if namespace.is_empty() {
                return Err(SchemaDecodeError::empty_custom_enum_namespace(name));
            }
            let identity = crate::schema::enum_identity(name, Some(namespace.as_str()));
            Ok(AttributeType::enum_with_base(
                identity,
                protocol_to_attribute_type(base)?,
                None,
                Vec::new(),
                None,
                dsl_transform.clone(),
            ))
        }
        protocol::AttributeType::Ref { name } => Ok(AttributeType::ref_(name.clone())),
    }
}

fn decode_optional_identity(identity: &Option<String>) -> Option<TypeIdentity> {
    identity
        .as_deref()
        .filter(|identity| !identity.is_empty())
        .map(protocol_to_type_identity)
}

fn reject_opaque_validator(
    kind: &str,
    validate: &Option<serde_json::Value>,
) -> Result<(), SchemaDecodeError> {
    if validate.is_some() {
        Err(SchemaDecodeError::opaque_validator(kind))
    } else {
        Ok(())
    }
}

fn decoded_refined_string(
    identity: Option<TypeIdentity>,
    pattern: Option<String>,
    length: Option<(Option<u64>, Option<u64>)>,
    to_dsl: Option<protocol::DslTransform>,
) -> AttributeType {
    match identity {
        Some(identity) => match crate::schema::builtin_validator(
            crate::schema::RefinedScalarKind::String,
            &identity,
        ) {
            Some(validate) => AttributeType::refined_string_with_validator(
                identity, pattern, length, validate, to_dsl,
            ),
            None => AttributeType::refined_string(Some(identity), pattern, length, to_dsl),
        },
        None => AttributeType::refined_string(None, pattern, length, to_dsl),
    }
}

fn decoded_refined_int(
    identity: Option<TypeIdentity>,
    range: Option<(Option<i64>, Option<i64>)>,
) -> AttributeType {
    match identity {
        Some(identity) => {
            match crate::schema::builtin_validator(crate::schema::RefinedScalarKind::Int, &identity)
            {
                Some(validate) => {
                    AttributeType::refined_int_with_validator(identity, range, validate)
                }
                None => AttributeType::refined_int(Some(identity), range),
            }
        }
        None => AttributeType::refined_int(None, range),
    }
}

fn decoded_refined_float(
    identity: Option<TypeIdentity>,
    range: Option<(Option<f64>, Option<f64>)>,
) -> AttributeType {
    match identity {
        Some(identity) => match crate::schema::builtin_validator(
            crate::schema::RefinedScalarKind::Float,
            &identity,
        ) {
            Some(validate) => {
                AttributeType::refined_float_with_validator(identity, range, validate)
            }
            None => AttributeType::refined_float(Some(identity), range),
        },
        None => AttributeType::refined_float(None, range),
    }
}

fn protocol_legacy_custom_to_core(
    enclosing_name: &str,
    base: &protocol::AttributeType,
    pattern: Option<String>,
    length: Option<(Option<u64>, Option<u64>)>,
    to_dsl: Option<protocol::DslTransform>,
) -> Result<AttributeType, SchemaDecodeError> {
    if let protocol::AttributeType::Custom {
        name: _inner_name,
        base: inner_base,
        pattern: inner_pattern,
        length: inner_length,
        validate: inner_validate,
        to_dsl: inner_to_dsl,
    } = base
    {
        reject_opaque_validator("Custom", inner_validate)?;
        // Released v12 providers nest generic Custom records (for example
        // `aws.ResourceId`) below the concrete attribute identity. The first
        // outer record exclusively owns the identity, even when its name is
        // empty; nested names only describe reusable bases.
        return protocol_legacy_custom_to_core(
            enclosing_name,
            inner_base,
            prefer_outer_legacy_custom_metadata(pattern, inner_pattern.clone()),
            prefer_outer_legacy_custom_metadata(length, *inner_length),
            prefer_outer_legacy_custom_metadata(to_dsl, inner_to_dsl.clone()),
        );
    }

    match base {
        protocol::AttributeType::String {
            pattern: base_pattern,
            length: base_length,
            validate: base_validate,
            to_dsl: base_to_dsl,
            identity: _base_identity,
        } => {
            reject_opaque_validator("String", base_validate)?;
            let identity = legacy_custom_identity(enclosing_name);
            let pattern = prefer_outer_legacy_custom_metadata(pattern, base_pattern.clone());
            let length = prefer_outer_legacy_custom_metadata(length, *base_length);
            let to_dsl = prefer_outer_legacy_custom_metadata(to_dsl, base_to_dsl.clone());
            Ok(decoded_refined_string(identity, pattern, length, to_dsl))
        }
        protocol::AttributeType::Int {
            range: base_range,
            identity: _base_identity,
        } => {
            reject_legacy_custom_metadata(enclosing_name, "pattern", pattern.as_ref(), "Int")?;
            reject_legacy_custom_metadata(enclosing_name, "to_dsl", to_dsl.as_ref(), "Int")?;
            let identity = legacy_custom_identity(enclosing_name);
            let outer_range = length
                .map(|(min, max)| {
                    Ok((
                        legacy_custom_int_bound(enclosing_name, min)?,
                        legacy_custom_int_bound(enclosing_name, max)?,
                    ))
                })
                .transpose()?;
            let range = prefer_outer_legacy_custom_metadata(outer_range, *base_range);
            Ok(decoded_refined_int(identity, range))
        }
        protocol::AttributeType::Float {
            range: base_range,
            identity: _base_identity,
        } => {
            reject_legacy_custom_metadata(enclosing_name, "pattern", pattern.as_ref(), "Float")?;
            reject_legacy_custom_metadata(enclosing_name, "length", length.as_ref(), "Float")?;
            reject_legacy_custom_metadata(enclosing_name, "to_dsl", to_dsl.as_ref(), "Float")?;
            let identity = legacy_custom_identity(enclosing_name);
            Ok(decoded_refined_float(identity, *base_range))
        }
        protocol::AttributeType::List {
            element_type,
            ordered,
            length: base_length,
            validate: base_validate,
        } => {
            reject_opaque_validator("List", base_validate)?;
            if !enclosing_name.is_empty() {
                return Err(SchemaDecodeError::unsupported_custom_base(
                    enclosing_name,
                    "List",
                ));
            }
            reject_legacy_custom_metadata(enclosing_name, "pattern", pattern.as_ref(), "List")?;
            reject_legacy_custom_metadata(enclosing_name, "to_dsl", to_dsl.as_ref(), "List")?;
            Ok(AttributeType::refined_list(
                protocol_to_attribute_type(element_type)?,
                *ordered,
                prefer_outer_legacy_custom_metadata(length, *base_length),
            ))
        }
        protocol::AttributeType::Bool => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "Bool",
        )),
        protocol::AttributeType::Duration => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "Duration",
        )),
        protocol::AttributeType::StringEnum {
            values: _base_values,
            name: _base_name,
            namespace: _base_namespace,
            dsl_aliases: _base_dsl_aliases,
        } => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "StringEnum",
        )),
        protocol::AttributeType::Map {
            inner: _base_inner,
            key: _base_key,
        } => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "Map",
        )),
        protocol::AttributeType::Struct {
            name: _base_name,
            fields: _base_fields,
            field_groups: _base_field_groups,
        } => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "Struct",
        )),
        protocol::AttributeType::Union {
            members: _base_members,
        } => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "Union",
        )),
        protocol::AttributeType::Custom {
            name: _base_name,
            base: _base,
            pattern: _base_pattern,
            length: _base_length,
            validate: _base_validate,
            to_dsl: _base_to_dsl,
        } => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "Custom",
        )),
        protocol::AttributeType::CustomEnum {
            name: _base_name,
            base: _base,
            namespace: _base_namespace,
            dsl_transform: _base_dsl_transform,
        } => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            "CustomEnum",
        )),
        protocol::AttributeType::Ref { name: _base_name } => Err(
            SchemaDecodeError::unsupported_custom_base(enclosing_name, "Ref"),
        ),
    }
}

fn legacy_custom_identity(enclosing_name: &str) -> Option<TypeIdentity> {
    (!enclosing_name.is_empty()).then(|| TypeIdentity::from_dotted(enclosing_name))
}

fn prefer_outer_legacy_custom_metadata<T>(enclosing: Option<T>, base: Option<T>) -> Option<T> {
    match (enclosing, base) {
        (Some(value), _) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn reject_legacy_custom_metadata<T>(
    identity: &str,
    field: &'static str,
    value: Option<&T>,
    base_kind: &'static str,
) -> Result<(), SchemaDecodeError> {
    if value.is_some() {
        Err(SchemaDecodeError::protocol(
            ProtocolConvertError::LegacyCustomUnsupportedMetadata {
                identity: identity.to_string(),
                field,
                base_kind,
            },
            "legacy Custom type carries metadata incompatible with its base",
        ))
    } else {
        Ok(())
    }
}

fn legacy_custom_int_bound(
    enclosing_name: &str,
    bound: Option<u64>,
) -> Result<Option<i64>, SchemaDecodeError> {
    bound
        .map(|value| {
            i64::try_from(value).map_err(|_error| {
                SchemaDecodeError::legacy_custom_int_bound_out_of_range(enclosing_name, value)
            })
        })
        .transpose()
}

pub fn protocol_to_struct_field(
    field: &protocol::StructField,
) -> Result<StructField, SchemaDecodeError> {
    let protocol::StructField {
        name,
        field_type,
        required,
        description,
        block_name,
        provider_name,
        read_only,
        write_only,
        deferred_populate,
    } = field;
    let input_mode = protocol_to_input_mode(*required, *read_only)
        .map_err(|error| error.prepend_attribute(name))?;
    if input_mode == InputMode::ProviderPopulated && *write_only {
        return Err(SchemaDecodeError::conflicting_write_only_mode().prepend_attribute(name));
    }
    let field_type =
        protocol_to_attribute_type(field_type).map_err(|error| error.prepend_attribute(name))?;

    match input_mode {
        InputMode::Optional => {
            let mut builder = StructField::optional(name.clone(), field_type);
            if let Some(description) = description {
                builder = builder.with_description(description.clone());
            }
            if let Some(provider_name) = provider_name {
                builder = builder.with_provider_name(provider_name.clone());
            }
            if let Some(block_name) = block_name {
                builder = builder.with_block_name(block_name.clone());
            }
            if *write_only {
                builder = builder.write_only();
            }
            if *deferred_populate {
                builder = builder.deferred_populate();
            }
            Ok(builder.build())
        }
        InputMode::Required => {
            let mut builder = StructField::required(name.clone(), field_type);
            if let Some(description) = description {
                builder = builder.with_description(description.clone());
            }
            if let Some(provider_name) = provider_name {
                builder = builder.with_provider_name(provider_name.clone());
            }
            if let Some(block_name) = block_name {
                builder = builder.with_block_name(block_name.clone());
            }
            if *write_only {
                builder = builder.write_only();
            }
            if *deferred_populate {
                builder = builder.deferred_populate();
            }
            Ok(builder.build())
        }
        InputMode::ProviderPopulated => {
            let mut builder = StructField::provider_populated(name.clone(), field_type);
            if let Some(description) = description {
                builder = builder.with_description(description.clone());
            }
            if let Some(provider_name) = provider_name {
                builder = builder.with_provider_name(provider_name.clone());
            }
            if let Some(block_name) = block_name {
                builder = builder.with_block_name(block_name.clone());
            }
            if *deferred_populate {
                builder = builder.deferred_populate();
            }
            Ok(builder.build())
        }
    }
}

fn protocol_to_input_mode(required: bool, read_only: bool) -> Result<InputMode, SchemaDecodeError> {
    match (required, read_only) {
        (false, false) => Ok(InputMode::Optional),
        (true, false) => Ok(InputMode::Required),
        (false, true) => Ok(InputMode::ProviderPopulated),
        (true, true) => Err(SchemaDecodeError::conflicting_input_mode()),
    }
}

fn input_mode_name(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Optional => "optional user-input",
        InputMode::Required => "required user-input",
        InputMode::ProviderPopulated => "provider-populated",
    }
}

/// Convert an ordinary concrete core value to the protocol value domain.
pub fn try_core_to_protocol_value(value: &Value) -> Result<protocol::Value, ProtocolConvertError> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Ok(protocol::Value::String(
            value.expose_plaintext().to_string(),
        )),
        Value::Concrete(ConcreteValue::EnumIdentifier(_identifier)) => {
            Err(ProtocolConvertError::NonWireValue {
                variant: "EnumIdentifier",
            })
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
            Ok(protocol::Value::String(value.api_value().to_string()))
        }
        Value::Concrete(ConcreteValue::Int(value)) => Ok(protocol::Value::Int(*value)),
        Value::Concrete(ConcreteValue::Float(value)) => {
            if value.is_finite() {
                Ok(protocol::Value::Float(*value))
            } else {
                Err(ProtocolConvertError::NonFiniteFloat)
            }
        }
        Value::Concrete(ConcreteValue::Bool(value)) => Ok(protocol::Value::Bool(*value)),
        Value::Concrete(ConcreteValue::Duration(value)) => i64::try_from(value.as_secs())
            .map(protocol::Value::Int)
            .map_err(|_error| ProtocolConvertError::DurationOutOfRange),
        Value::Concrete(ConcreteValue::List(values)) => Ok(protocol::Value::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    try_core_to_protocol_value(value)
                        .map_err(|error| error.context(format!("list index {index}")))
                })
                .collect::<Result<_, _>>()?,
        )),
        Value::Concrete(ConcreteValue::StringList(values)) => Ok(protocol::Value::StringList(
            values
                .iter()
                .map(|value| value.expose_plaintext().to_string())
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(values)) => Ok(protocol::Value::Map(
            values
                .iter()
                .map(|(key, value)| {
                    try_core_to_protocol_value(value)
                        .map(|converted| (key.clone(), converted))
                        .map_err(|error| error.context(format!("map key `{key}`")))
                })
                .collect::<Result<_, _>>()?,
        )),
        Value::Deferred(deferred) => Err(ProtocolConvertError::NonWireValue {
            variant: deferred_value_variant(deferred),
        }),
    }
}

fn deferred_value_variant(value: &DeferredValue) -> &'static str {
    match value {
        DeferredValue::ResourceRef { path: _path } => "ResourceRef",
        DeferredValue::BindingRef { binding: _binding } => "BindingRef",
        DeferredValue::Coalesce {
            value: _value,
            default: _default,
        } => "Coalesce",
        DeferredValue::Interpolation(_parts) => "Interpolation",
        DeferredValue::FunctionCall {
            name: _name,
            args: _args,
            source_line: _source_line,
        } => "FunctionCall",
        DeferredValue::Unavailable {
            name: _name,
            args: _args,
            reason: _reason,
            source_line: _source_line,
        } => "Unavailable",
        DeferredValue::ResolvedFunctionCall {
            function: _function,
            functions: _functions,
            args: _args,
            source_line: _source_line,
        } => "ResolvedFunctionCall",
        DeferredValue::Access(_access) => "Access",
        DeferredValue::Secret(_inner) => "Secret",
        DeferredValue::RedactedSecret { hash: _hash } => "RedactedSecret",
        DeferredValue::Unknown(_reason) => "Unknown",
        DeferredValue::Placeholder(never) => match *never {},
    }
}

/// Decode an ordinary protocol value while preserving the typed StringList variant.
pub fn protocol_to_core_value(value: &protocol::Value) -> Result<Value, ProtocolConvertError> {
    match value {
        protocol::Value::Bool(value) => Ok(Value::Concrete(ConcreteValue::Bool(*value))),
        protocol::Value::Int(value) => Ok(Value::Concrete(ConcreteValue::Int(*value))),
        protocol::Value::Float(value) => {
            if value.is_finite() {
                Ok(Value::Concrete(ConcreteValue::Float(*value)))
            } else {
                Err(ProtocolConvertError::NonFiniteFloat)
            }
        }
        protocol::Value::String(value) => {
            Ok(Value::Concrete(ConcreteValue::String(value.clone().into())))
        }
        protocol::Value::StringList(values) => Ok(Value::Concrete(ConcreteValue::StringList(
            values.iter().cloned().map(Into::into).collect(),
        ))),
        protocol::Value::List(values) => Ok(Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    protocol_to_core_value(value)
                        .map_err(|error| error.context(format!("list index {index}")))
                })
                .collect::<Result<_, _>>()?,
        ))),
        protocol::Value::Map(values) => Ok(Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(key, value)| {
                    protocol_to_core_value(value)
                        .map(|converted| (key.clone(), converted))
                        .map_err(|error| error.context(format!("map key `{key}`")))
                })
                .collect::<Result<IndexMap<_, _>, _>>()?,
        ))),
    }
}

/// Decode a schema default through the JSON-compatible portable projection.
/// Both protocol list variants become a core `List`, because JSON arrays do
/// not retain the protocol's in-memory StringList tag.
pub fn protocol_schema_default_to_core(
    value: &protocol::Value,
) -> Result<Value, ProtocolConvertError> {
    match value {
        protocol::Value::Bool(value) => Ok(Value::Concrete(ConcreteValue::Bool(*value))),
        protocol::Value::Int(value) => Ok(Value::Concrete(ConcreteValue::Int(*value))),
        protocol::Value::Float(value) => {
            if value.is_finite() {
                Ok(Value::Concrete(ConcreteValue::Float(*value)))
            } else {
                Err(ProtocolConvertError::NonFiniteFloat)
            }
        }
        protocol::Value::String(value) => {
            Ok(Value::Concrete(ConcreteValue::String(value.clone().into())))
        }
        protocol::Value::StringList(values) => Ok(Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .cloned()
                .map(|value| Value::Concrete(ConcreteValue::String(value.into())))
                .collect(),
        ))),
        protocol::Value::List(values) => Ok(Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    protocol_schema_default_to_core(value)
                        .map_err(|error| error.context(format!("list index {index}")))
                })
                .collect::<Result<_, _>>()?,
        ))),
        protocol::Value::Map(values) => Ok(Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(key, value)| {
                    protocol_schema_default_to_core(value)
                        .map(|converted| (key.clone(), converted))
                        .map_err(|error| error.context(format!("map key `{key}`")))
                })
                .collect::<Result<IndexMap<_, _>, _>>()?,
        ))),
    }
}

/// Convert an ordinary core value map to the provider protocol domain.
pub fn try_core_to_protocol_value_map(
    values: &HashMap<String, Value>,
) -> Result<BTreeMap<String, protocol::Value>, ProtocolConvertError> {
    values
        .iter()
        .map(|(key, value)| {
            try_core_to_protocol_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("map key `{key}`")))
        })
        .collect()
}

/// Decode an ordinary provider protocol value map.
pub fn protocol_to_core_value_map(
    values: &BTreeMap<String, protocol::Value>,
) -> Result<HashMap<String, Value>, ProtocolConvertError> {
    values
        .iter()
        .map(|(key, value)| {
            protocol_to_core_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("map key `{key}`")))
        })
        .collect()
}

/// Convert a value at a persisted-state position. Only this typed boundary
/// may transport a redacted-secret digest.
pub fn try_core_to_protocol_persisted_value(
    value: &Value,
) -> Result<protocol::PersistedValue, ProtocolConvertError> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Ok(protocol::PersistedValue::String(
            value.expose_plaintext().to_string(),
        )),
        Value::Concrete(ConcreteValue::EnumIdentifier(_identifier)) => {
            Err(ProtocolConvertError::NonWireValue {
                variant: "EnumIdentifier",
            })
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(value)) => Ok(
            protocol::PersistedValue::String(value.api_value().to_string()),
        ),
        Value::Concrete(ConcreteValue::Int(value)) => Ok(protocol::PersistedValue::Int(*value)),
        Value::Concrete(ConcreteValue::Float(value)) => {
            if value.is_finite() {
                Ok(protocol::PersistedValue::Float(*value))
            } else {
                Err(ProtocolConvertError::NonFiniteFloat)
            }
        }
        Value::Concrete(ConcreteValue::Bool(value)) => Ok(protocol::PersistedValue::Bool(*value)),
        Value::Concrete(ConcreteValue::Duration(value)) => i64::try_from(value.as_secs())
            .map(protocol::PersistedValue::Int)
            .map_err(|_error| ProtocolConvertError::DurationOutOfRange),
        Value::Concrete(ConcreteValue::List(values)) => Ok(protocol::PersistedValue::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    try_core_to_protocol_persisted_value(value)
                        .map_err(|error| error.context(format!("list index {index}")))
                })
                .collect::<Result<_, _>>()?,
        )),
        Value::Concrete(ConcreteValue::StringList(values)) => {
            Ok(protocol::PersistedValue::StringList(
                values
                    .iter()
                    .map(|value| value.expose_plaintext().to_string())
                    .collect(),
            ))
        }
        Value::Concrete(ConcreteValue::Map(values)) => Ok(protocol::PersistedValue::Map(
            values
                .iter()
                .map(|(key, value)| {
                    try_core_to_protocol_persisted_value(value)
                        .map(|converted| (key.clone(), converted))
                        .map_err(|error| error.context(format!("map key `{key}`")))
                })
                .collect::<Result<_, _>>()?,
        )),
        Value::Deferred(DeferredValue::RedactedSecret { hash }) => Ok(
            protocol::PersistedValue::RedactedSecret(hash.encoded_digest()),
        ),
        Value::Deferred(deferred) => Err(ProtocolConvertError::NonWireValue {
            variant: deferred_value_variant(deferred),
        }),
    }
}

/// Decode a value from a persisted-state position.
pub fn protocol_to_core_persisted_value(
    value: &protocol::PersistedValue,
) -> Result<Value, ProtocolConvertError> {
    match value {
        protocol::PersistedValue::Bool(value) => Ok(Value::Concrete(ConcreteValue::Bool(*value))),
        protocol::PersistedValue::Int(value) => Ok(Value::Concrete(ConcreteValue::Int(*value))),
        protocol::PersistedValue::Float(value) => {
            if value.is_finite() {
                Ok(Value::Concrete(ConcreteValue::Float(*value)))
            } else {
                Err(ProtocolConvertError::NonFiniteFloat)
            }
        }
        protocol::PersistedValue::String(value) => {
            Ok(Value::Concrete(ConcreteValue::String(value.clone().into())))
        }
        protocol::PersistedValue::RedactedSecret(digest) => {
            let hash = RedactedSecretHash::from_encoded_digest(digest)
                .ok_or(ProtocolConvertError::InvalidRedactedSecretDigest)?;
            Ok(Value::Deferred(DeferredValue::RedactedSecret { hash }))
        }
        protocol::PersistedValue::StringList(values) => Ok(Value::Concrete(
            ConcreteValue::StringList(values.iter().cloned().map(Into::into).collect()),
        )),
        protocol::PersistedValue::List(values) => Ok(Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    protocol_to_core_persisted_value(value)
                        .map_err(|error| error.context(format!("list index {index}")))
                })
                .collect::<Result<_, _>>()?,
        ))),
        protocol::PersistedValue::Map(values) => Ok(Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(key, value)| {
                    protocol_to_core_persisted_value(value)
                        .map(|converted| (key.clone(), converted))
                        .map_err(|error| error.context(format!("map key `{key}`")))
                })
                .collect::<Result<IndexMap<_, _>, _>>()?,
        ))),
    }
}

/// Convert a core map at a persisted-state position.
pub fn try_core_to_protocol_persisted_value_map(
    values: &HashMap<String, Value>,
) -> Result<BTreeMap<String, protocol::PersistedValue>, ProtocolConvertError> {
    values
        .iter()
        .map(|(key, value)| {
            try_core_to_protocol_persisted_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("map key `{key}`")))
        })
        .collect()
}

/// Decode a persisted protocol value map.
pub fn protocol_to_core_persisted_value_map(
    values: &BTreeMap<String, protocol::PersistedValue>,
) -> Result<HashMap<String, Value>, ProtocolConvertError> {
    values
        .iter()
        .map(|(key, value)| {
            protocol_to_core_persisted_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("map key `{key}`")))
        })
        .collect()
}

/// Encode a core resource id. Provider-instance routing is host-local and the
/// version-13 protocol deliberately has no corresponding field.
pub fn resource_id_to_protocol(id: &ResourceId) -> protocol::ResourceId {
    let ResourceId {
        provider,
        resource_type,
        identity,
        provider_instance: _provider_instance,
    } = id;
    let identity = match identity {
        ResourceIdentityState::Pending(_pending) => String::new(),
        ResourceIdentityState::Resolved(identity) => identity.as_str().to_string(),
    };
    protocol::ResourceId {
        provider: provider.clone(),
        resource_type: resource_type.clone(),
        identity,
    }
}

/// Decode a version-13 resource id. The wire's empty identity is the pending
/// sentinel, and provider-instance routing cannot be reconstructed.
pub fn protocol_to_resource_id(id: &protocol::ResourceId) -> ResourceId {
    let protocol::ResourceId {
        provider,
        resource_type,
        identity,
    } = id;
    match ResourceIdentity::try_from(identity.clone()) {
        Ok(identity) => ResourceId::with_provider_identity(
            provider.clone(),
            resource_type.clone(),
            identity,
            None,
        ),
        Err(ResourceIdentityError::Empty) => {
            ResourceId::pending_with_provider(provider.clone(), resource_type.clone(), None)
        }
    }
}

/// Encode the directive subset transported to providers.
pub fn directives_to_protocol(directives: &Directives) -> protocol::Directives {
    let Directives {
        force_delete,
        create_before_destroy,
        prevent_destroy,
        depends_on: _depends_on,
        // Host-only lifecycle metadata. Providers must never receive authored
        // ignore paths; the host resolves and enforces them around CRUD calls.
        ignore_changes: _ignore_changes,
        provider_instance: _provider_instance,
    } = directives;
    protocol::Directives {
        force_delete: *force_delete,
        create_before_destroy: *create_before_destroy,
        prevent_destroy: *prevent_destroy,
    }
}

/// Decode provider-visible directives, defaulting every host-only field.
pub fn protocol_to_directives(directives: &protocol::Directives) -> Directives {
    let protocol::Directives {
        force_delete,
        create_before_destroy,
        prevent_destroy,
    } = directives;
    Directives {
        force_delete: *force_delete,
        create_before_destroy: *create_before_destroy,
        prevent_destroy: *prevent_destroy,
        depends_on: Vec::new(),
        ignore_changes: Vec::new(),
        provider_instance: None,
    }
}

/// Encode a managed resource for the ordinary provider boundary.
pub fn try_resource_to_protocol<S>(
    resource: &Resource<S>,
) -> Result<protocol::Resource, ProtocolConvertError> {
    let Resource {
        id,
        attributes,
        directives,
        // Host-only lifecycle metadata resolved and enforced around provider
        // calls. It is intentionally absent from the WIT resource record.
        resolved_ignore_changes: _resolved_ignore_changes,
        prefixes: _prefixes,
        binding: _binding,
        dependency_bindings: _dependency_bindings,
        module_source: _module_source,
        default_tag_keys: _default_tag_keys,
        loop_provenance: _loop_provenance,
        quoted_string_attrs: _quoted_string_attrs,
    } = resource;
    let attributes = attributes
        .iter()
        .map(|(key, value)| {
            try_core_to_protocol_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("attribute `{key}`")))
        })
        .collect::<Result<_, _>>()?;
    Ok(protocol::Resource {
        id: resource_id_to_protocol(id),
        attributes,
        directives: directives_to_protocol(directives),
    })
}

/// Encode a data source using the protocol's shared Resource record.
pub fn try_data_source_to_protocol(
    data_source: &DataSource,
) -> Result<protocol::Resource, ProtocolConvertError> {
    let DataSource {
        id,
        attributes,
        directives,
        binding: _binding,
        dependency_bindings: _dependency_bindings,
        module_source: _module_source,
        quoted_string_attrs: _quoted_string_attrs,
    } = data_source;
    let attributes = attributes
        .iter()
        .map(|(key, value)| {
            try_core_to_protocol_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("attribute `{key}`")))
        })
        .collect::<Result<_, _>>()?;
    Ok(protocol::Resource {
        id: resource_id_to_protocol(id),
        attributes,
        directives: directives_to_protocol(directives),
    })
}

/// Decode a protocol Resource as a managed resource.
pub fn protocol_to_resource(
    resource: &protocol::Resource,
) -> Result<Resource, ProtocolConvertError> {
    let protocol::Resource {
        id,
        attributes,
        directives,
    } = resource;
    let mut core = Resource::from_id(protocol_to_resource_id(id));
    core.attributes = attributes
        .iter()
        .map(|(key, value)| {
            protocol_to_core_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("attribute `{key}`")))
        })
        .collect::<Result<IndexMap<_, _>, _>>()?;
    core.directives = protocol_to_directives(directives);
    Ok(core)
}

/// Decode a protocol Resource as a data source.
pub fn protocol_to_data_source(
    resource: &protocol::Resource,
) -> Result<DataSource, ProtocolConvertError> {
    let protocol::Resource {
        id,
        attributes,
        directives,
    } = resource;
    let mut core =
        DataSource::pending_with_provider(id.provider.clone(), id.resource_type.clone(), None);
    core.id = protocol_to_resource_id(id);
    core.attributes = attributes
        .iter()
        .map(|(key, value)| {
            protocol_to_core_value(value)
                .map(|converted| (key.clone(), converted))
                .map_err(|error| error.context(format!("attribute `{key}`")))
        })
        .collect::<Result<IndexMap<_, _>, _>>()?;
    core.directives = protocol_to_directives(directives);
    Ok(core)
}

/// Encode generic state through the ordinary value domain.
pub fn try_state_to_protocol(state: &State) -> Result<protocol::State, ProtocolConvertError> {
    let State {
        id,
        identifier,
        attributes,
        exists,
        dependency_bindings: _dependency_bindings,
        partial_read: _partial_read,
    } = state;
    Ok(protocol::State {
        id: resource_id_to_protocol(id),
        identifier: identifier.clone(),
        attributes: try_core_to_protocol_value_map(attributes)?,
        exists: *exists,
    })
}

/// Decode generic ordinary-value state.
pub fn protocol_to_state(state: &protocol::State) -> Result<State, ProtocolConvertError> {
    let protocol::State {
        id,
        identifier,
        attributes,
        exists,
    } = state;
    if *exists {
        let mut core = State::existing(
            protocol_to_resource_id(id),
            protocol_to_core_value_map(attributes)?,
        );
        if let Some(identifier) = identifier {
            core = core.with_identifier(identifier.clone());
        }
        Ok(core)
    } else {
        Ok(State::not_found(protocol_to_resource_id(id)))
    }
}

/// Encode generic state through the persisted value domain.
pub fn try_state_to_persisted_protocol(
    state: &State,
) -> Result<protocol::PersistedState, ProtocolConvertError> {
    let State {
        id,
        identifier,
        attributes,
        exists,
        dependency_bindings: _dependency_bindings,
        partial_read: _partial_read,
    } = state;
    Ok(protocol::PersistedState {
        id: resource_id_to_protocol(id),
        identifier: identifier.clone(),
        attributes: try_core_to_protocol_persisted_value_map(attributes)?,
        exists: *exists,
    })
}

/// Decode generic persisted state.
pub fn protocol_persisted_state_to_core(
    state: &protocol::PersistedState,
) -> Result<State, ProtocolConvertError> {
    let protocol::PersistedState {
        id,
        identifier,
        attributes,
        exists,
    } = state;
    if *exists {
        let mut core = State::existing(
            protocol_to_resource_id(id),
            protocol_to_core_persisted_value_map(attributes)?,
        );
        if let Some(identifier) = identifier {
            core = core.with_identifier(identifier.clone());
        }
        Ok(core)
    } else {
        Ok(State::not_found(protocol_to_resource_id(id)))
    }
}

/// Encode state known to exist through the ordinary value domain.
pub fn try_applied_state_to_protocol(
    state: &AppliedState,
) -> Result<protocol::AppliedState, ProtocolConvertError> {
    let AppliedState {
        id,
        identifier,
        attributes,
    } = state;
    Ok(protocol::AppliedState {
        id: resource_id_to_protocol(id),
        identifier: identifier.clone(),
        attributes: try_core_to_protocol_value_map(attributes)?,
    })
}

/// Decode state known to exist from the ordinary value domain.
pub fn protocol_applied_state_to_core(
    state: &protocol::AppliedState,
) -> Result<AppliedState, ProtocolConvertError> {
    let protocol::AppliedState {
        id,
        identifier,
        attributes,
    } = state;
    Ok(AppliedState::from_identifier(
        protocol_to_resource_id(id),
        identifier.clone(),
        protocol_to_core_value_map(attributes)?,
    ))
}

/// Encode a managed read outcome.
pub fn try_read_outcome_to_protocol(
    outcome: &ReadOutcome,
) -> Result<protocol::ReadOutcome, ProtocolConvertError> {
    match outcome {
        ReadOutcome::NotFound { id } => Ok(protocol::ReadOutcome::NotFound {
            id: resource_id_to_protocol(id),
        }),
        ReadOutcome::Found { state } => Ok(protocol::ReadOutcome::Found {
            state: try_applied_state_to_protocol(state)?,
        }),
    }
}

/// Decode a managed read outcome.
pub fn protocol_read_outcome_to_core(
    outcome: &protocol::ReadOutcome,
) -> Result<ReadOutcome, ProtocolConvertError> {
    match outcome {
        protocol::ReadOutcome::NotFound { id } => {
            Ok(ReadOutcome::not_found(protocol_to_resource_id(id)))
        }
        protocol::ReadOutcome::Found { state } => {
            Ok(ReadOutcome::found(protocol_applied_state_to_core(state)?))
        }
    }
}

fn try_applied_update_state_to_protocol(
    state: &AppliedState,
    state_after_patch: protocol::PersistedState,
) -> Result<protocol::AppliedPersistedState, ProtocolConvertError> {
    let AppliedState {
        id,
        identifier,
        attributes: state_attributes,
    } = state;
    let protocol::PersistedState {
        id: _state_after_patch_id,
        identifier: _state_after_patch_identifier,
        attributes: state_after_patch_attributes,
        exists: _state_after_patch_exists,
    } = state_after_patch;
    let mut attributes = try_core_to_protocol_persisted_value_map(state_attributes)?;
    for (key, value) in state_after_patch_attributes {
        attributes.entry(key).or_insert(value);
    }
    Ok(protocol::AppliedPersistedState {
        id: resource_id_to_protocol(id),
        identifier: identifier.clone(),
        attributes,
    })
}

/// Encode an update outcome, retaining persisted write-only values from the
/// state captured immediately after applying the patch.
pub fn try_update_outcome_to_protocol(
    outcome: &UpdateOutcome,
    state_after_patch: protocol::PersistedState,
) -> Result<protocol::UpdateOutcome, ProtocolConvertError> {
    match outcome {
        UpdateOutcome::Success { state } => Ok(protocol::UpdateOutcome::Success {
            state: try_applied_update_state_to_protocol(state, state_after_patch)?,
        }),
        UpdateOutcome::PartialSuccess { state, diagnostic } => {
            let PartialReadDiagnostic {
                reason,
                missing_attributes,
            } = diagnostic;
            Ok(protocol::UpdateOutcome::PartialSuccess {
                state: try_applied_update_state_to_protocol(state, state_after_patch)?,
                diagnostic: protocol::PartialReadDiagnostic {
                    reason: reason.clone(),
                    missing_attributes: missing_attributes.clone(),
                },
            })
        }
    }
}

fn protocol_applied_persisted_state_to_core(
    state: &protocol::AppliedPersistedState,
) -> Result<AppliedState, ProtocolConvertError> {
    let protocol::AppliedPersistedState {
        id,
        identifier,
        attributes,
    } = state;
    Ok(AppliedState::from_identifier(
        protocol_to_resource_id(id),
        identifier.clone(),
        protocol_to_core_persisted_value_map(attributes)?,
    ))
}

/// Decode an update outcome. An empty partial-read attribute list normalizes
/// to core's success variant through [`UpdateOutcome::partial_success`].
pub fn protocol_update_outcome_to_core(
    outcome: &protocol::UpdateOutcome,
) -> Result<UpdateOutcome, ProtocolConvertError> {
    match outcome {
        protocol::UpdateOutcome::Success { state } => Ok(UpdateOutcome::Success {
            state: protocol_applied_persisted_state_to_core(state)?,
        }),
        protocol::UpdateOutcome::PartialSuccess { state, diagnostic } => {
            let protocol::PartialReadDiagnostic {
                reason,
                missing_attributes,
            } = diagnostic;
            Ok(UpdateOutcome::partial_success(
                protocol_applied_persisted_state_to_core(state)?,
                reason.clone(),
                missing_attributes.clone(),
            ))
        }
    }
}

/// Encode one update patch operation.
pub fn try_patch_op_to_protocol(
    operation: &PatchOp,
) -> Result<protocol::PatchOp, ProtocolConvertError> {
    match operation {
        PatchOp::Add { key, value } => Ok(protocol::PatchOp::Add {
            key: key.clone(),
            value: try_core_to_protocol_value(value)
                .map_err(|error| error.context(format!("patch add `{key}`")))?,
        }),
        PatchOp::Replace { key, value } => Ok(protocol::PatchOp::Replace {
            key: key.clone(),
            value: try_core_to_protocol_value(value)
                .map_err(|error| error.context(format!("patch replace `{key}`")))?,
        }),
        PatchOp::Reassert { key, value } => Ok(protocol::PatchOp::Reassert {
            key: key.clone(),
            value: try_core_to_protocol_value(value)
                .map_err(|error| error.context(format!("patch reassert `{key}`")))?,
        }),
        PatchOp::Remove { key } => Ok(protocol::PatchOp::Remove { key: key.clone() }),
    }
}

/// Decode one update patch operation.
pub fn protocol_patch_op_to_core(
    operation: &protocol::PatchOp,
) -> Result<PatchOp, ProtocolConvertError> {
    match operation {
        protocol::PatchOp::Add { key, value } => Ok(PatchOp::Add {
            key: key.clone(),
            value: protocol_to_core_value(value)
                .map_err(|error| error.context(format!("patch add `{key}`")))?,
        }),
        protocol::PatchOp::Replace { key, value } => Ok(PatchOp::Replace {
            key: key.clone(),
            value: protocol_to_core_value(value)
                .map_err(|error| error.context(format!("patch replace `{key}`")))?,
        }),
        protocol::PatchOp::Reassert { key, value } => Ok(PatchOp::Reassert {
            key: key.clone(),
            value: protocol_to_core_value(value)
                .map_err(|error| error.context(format!("patch reassert `{key}`")))?,
        }),
        protocol::PatchOp::Remove { key } => Ok(PatchOp::Remove { key: key.clone() }),
    }
}

/// Encode an update patch without changing operation order.
pub fn try_update_patch_to_protocol(
    patch: &UpdatePatch,
) -> Result<protocol::UpdatePatch, ProtocolConvertError> {
    let UpdatePatch { ops } = patch;
    Ok(protocol::UpdatePatch {
        ops: ops
            .iter()
            .map(try_patch_op_to_protocol)
            .collect::<Result<_, _>>()?,
    })
}

/// Decode an update patch without changing operation order.
pub fn protocol_update_patch_to_core(
    patch: &protocol::UpdatePatch,
) -> Result<UpdatePatch, ProtocolConvertError> {
    let protocol::UpdatePatch { ops } = patch;
    Ok(UpdatePatch {
        ops: ops
            .iter()
            .map(protocol_patch_op_to_core)
            .collect::<Result<_, _>>()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::UpdatePatch;
    use crate::resource::{ModuleSource, ProviderResource, RedactedSecretHash};
    use crate::schema::{
        CompletionValue, ProviderSchemaValidationError, ReferenceForm, ReferenceForms,
        ReferenceTemplate, ValidatedProviderSchemaWitness, ValidatorType,
    };

    fn reference_identity(kind: &str) -> TypeIdentity {
        TypeIdentity::new(Some("aws"), ["lambda", "Function"], kind)
    }

    fn reference_form() -> ReferenceForm {
        ReferenceForm::Template {
            long: ReferenceTemplate::new(
                "arn:{partition}:lambda:{region}:{account}:function:{name}",
            )
            .unwrap(),
        }
    }

    fn schema_with_identified_type(
        identity: &TypeIdentity,
        attribute_type: AttributeType,
    ) -> ValidatedResourceSchema {
        ResourceSchema::new("aws.lambda.Function")
            .attribute(AttributeSchema::optional("function", attribute_type).build())
            .validate()
            .unwrap_or_else(|error| panic!("fixture for {identity} must validate: {error}"))
    }

    fn protocol_identity(identity: &TypeIdentity) -> String {
        identity.to_string()
    }

    fn envelope_with_form(
        identity: &TypeIdentity,
        schema: Option<ValidatedResourceSchema>,
    ) -> protocol::ProviderSchemaEnvelope {
        protocol::ProviderSchemaEnvelope {
            resources: schema
                .iter()
                .map(resource_schema_to_protocol)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            reference_forms: vec![protocol::ReferenceFormDeclaration {
                identity: protocol_identity(identity),
                form: reference_form(),
            }],
        }
    }

    #[test]
    fn provider_schema_envelope_round_trips_the_aggregate_witness() {
        let first = reference_identity("Alias");
        let second = reference_identity("Name");
        let schema = ResourceSchema::new("aws.lambda.Function")
            .attribute(
                AttributeSchema::optional(
                    "alias",
                    AttributeType::refined_string(Some(first.clone()), None, None, None),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "name",
                    AttributeType::refined_string(Some(second.clone()), None, None, None),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let forms = ReferenceForms::from([
            (second.clone(), reference_form()),
            (first.clone(), reference_form()),
        ]);
        let config = ValidatedConfigSchema::default();
        let witness = ValidatedProviderSchemaWitness::new(vec![schema], config.clone(), forms)
            .expect("refined String forms must be admitted");

        let wire = provider_schema_to_protocol(&witness).expect("witness must encode");
        assert_eq!(
            wire.reference_forms
                .iter()
                .map(|record| record.identity.as_str())
                .collect::<Vec<_>>(),
            ["aws.lambda.Function.Alias", "aws.lambda.Function.Name"],
            "BTreeMap order is the wire order",
        );
        let json = serde_json::to_string(&wire).expect("envelope must serialize");
        let decoded_wire: protocol::ProviderSchemaEnvelope =
            serde_json::from_str(&json).expect("envelope must deserialize");
        let decoded = protocol_to_provider_schema_witness(&decoded_wire, config)
            .expect("envelope must regain its aggregate witness");

        assert_eq!(decoded.resources().len(), 1);
        assert_eq!(decoded.reference_forms().len(), 2);
        assert_eq!(
            decoded.reference_forms().get(&first),
            Some(&reference_form())
        );
        assert_eq!(
            decoded.reference_forms().get(&second),
            Some(&reference_form())
        );
    }

    #[test]
    fn provider_schema_envelope_decodes_form_and_attribute_identities_identically() {
        let identity = TypeIdentity::new(None::<String>, ["shared"], "Name");
        let schema = schema_with_identified_type(
            &identity,
            AttributeType::refined_string(Some(identity.clone()), None, None, None),
        );
        let witness = ValidatedProviderSchemaWitness::new(
            vec![schema],
            ValidatedConfigSchema::default(),
            ReferenceForms::from([(identity.clone(), reference_form())]),
        )
        .expect("the SDK-side aggregate identity is internally consistent");

        let wire = provider_schema_to_protocol(&witness).expect("witness must encode");
        let decoded = protocol_to_provider_schema_witness(&wire, ValidatedConfigSchema::default())
            .expect("forms and attributes must take the same lossy dotted identity path");
        let decoded_identity = TypeIdentity::from_dotted(&identity.to_string());

        assert_eq!(
            decoded.reference_forms().get(&decoded_identity),
            Some(&reference_form())
        );
    }

    #[test]
    fn provider_schema_decode_rejects_duplicate_reference_form_identities() {
        let identity = reference_identity("Name");
        let schema = schema_with_identified_type(
            &identity,
            AttributeType::refined_string(Some(identity.clone()), None, None, None),
        );
        let mut wire = envelope_with_form(&identity, Some(schema));
        wire.reference_forms.push(wire.reference_forms[0].clone());

        assert!(matches!(
            protocol_to_provider_schema_witness(&wire, ValidatedConfigSchema::default()),
            Err(SchemaDecodeError::DuplicateReferenceForm { identity: actual })
                if actual == identity
        ));
    }

    #[test]
    fn provider_schema_decode_rejects_dangling_reference_form_identities() {
        let identity = reference_identity("Name");
        let wire = envelope_with_form(&identity, None);

        assert!(matches!(
            protocol_to_provider_schema_witness(&wire, ValidatedConfigSchema::default()),
            Err(SchemaDecodeError::InvalidProviderSchema(
                ProviderSchemaValidationError::DanglingReferenceForm { identity: actual }
            )) if actual == identity
        ));
    }

    #[test]
    fn provider_schema_decode_accepts_a_form_declared_by_provider_config() {
        let identity = reference_identity("Endpoint");
        let config = ValidatedConfigSchema::validate(BTreeMap::from([(
            "endpoint".to_string(),
            AttributeType::refined_string(Some(identity.clone()), None, None, None),
        )]))
        .expect("config fixture must validate");
        let wire = envelope_with_form(&identity, None);

        let decoded = protocol_to_provider_schema_witness(&wire, config)
            .expect("config identities belong to the same aggregate witness");

        assert_eq!(
            decoded.reference_forms().get(&identity),
            Some(&reference_form())
        );
    }

    #[test]
    fn provider_schema_decode_rejects_int_reference_form_identities() {
        let identity = reference_identity("Count");
        let schema = schema_with_identified_type(
            &identity,
            AttributeType::refined_int(Some(identity.clone()), None),
        );
        let wire = envelope_with_form(&identity, Some(schema));

        assert!(matches!(
            protocol_to_provider_schema_witness(&wire, ValidatedConfigSchema::default()),
            Err(SchemaDecodeError::InvalidProviderSchema(
                ProviderSchemaValidationError::NonStringReferenceForm {
                    identity: actual,
                    declaration: crate::schema::IdentifiedDeclarationKind::Int,
                }
            )) if actual == identity
        ));
    }

    #[test]
    fn provider_schema_decode_rejects_enum_reference_form_identities() {
        let identity = reference_identity("Mode");
        let schema = schema_with_identified_type(
            &identity,
            AttributeType::enum_(
                identity.clone(),
                Some(vec!["enabled".to_string()]),
                Vec::new(),
                None,
                None,
            ),
        );
        let wire = envelope_with_form(&identity, Some(schema));

        assert!(matches!(
            protocol_to_provider_schema_witness(&wire, ValidatedConfigSchema::default()),
            Err(SchemaDecodeError::InvalidProviderSchema(
                ProviderSchemaValidationError::NonStringReferenceForm {
                    identity: actual,
                    declaration: crate::schema::IdentifiedDeclarationKind::Enum,
                }
            )) if actual == identity
        ));
    }

    #[test]
    fn provider_resource_encodes_identically_to_live_resource() {
        let mut resource = Resource::with_provider(
            "test",
            "test.Widget",
            ResourceIdentity::from("example"),
            None,
        );
        resource.attributes.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("widget".into())),
        );
        resource.directives.force_delete = true;
        resource.module_source = Some(ModuleSource::Root);
        let provider_resource = ProviderResource::from(resource.clone());

        let provider_wire =
            try_resource_to_protocol(&provider_resource).expect("provider resource must encode");
        let live_wire = try_resource_to_protocol(&resource).expect("live resource must encode");

        assert_eq!(
            serde_json::to_value(provider_wire).expect("provider protocol resource must serialize"),
            serde_json::to_value(live_wire).expect("live protocol resource must serialize")
        );
    }

    #[test]
    fn persisted_secret_uses_only_the_persisted_conversion_domain() {
        let digest = format!("argon2-legacy:73616c74:{}", "ab".repeat(32));
        let hash =
            RedactedSecretHash::from_encoded_digest(&digest).expect("fixture digest must be valid");
        let value = Value::Deferred(DeferredValue::RedactedSecret { hash });

        assert!(matches!(
            try_core_to_protocol_value(&value),
            Err(ProtocolConvertError::NonWireValue {
                variant: "RedactedSecret"
            })
        ));

        let encoded = try_core_to_protocol_persisted_value(&value)
            .expect("persisted positions may carry a redacted secret");
        assert_eq!(encoded, protocol::PersistedValue::RedactedSecret(digest));
        assert_eq!(
            protocol_to_core_persisted_value(&encoded).expect("persisted secret must decode"),
            value
        );
    }

    #[test]
    fn ordinary_decode_preserves_string_list_but_schema_defaults_normalize_it() {
        let wire = protocol::Value::StringList(vec!["one".to_string(), "two".to_string()]);

        assert!(matches!(
            protocol_to_core_value(&wire).unwrap(),
            Value::Concrete(ConcreteValue::StringList(values)) if values.len() == 2
        ));
        assert!(matches!(
            protocol_schema_default_to_core(&wire).unwrap(),
            Value::Concrete(ConcreteValue::List(values)) if values.len() == 2
        ));

        let nested = protocol::Value::Map(BTreeMap::from([(
            "items".to_string(),
            protocol::Value::List(vec![protocol::Value::Float(f64::INFINITY)]),
        )]));
        let error = protocol_schema_default_to_core(&nested)
            .expect_err("nested non-finite defaults must fail before JSON serialization");
        assert!(error.to_string().contains("map key `items`"));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn protocol_ordered_map_conversion_uses_canonical_key_order() {
        for iteration in 0..200 {
            let invalid = BTreeMap::from([
                (
                    "z_invalid".to_string(),
                    protocol::Value::Float(f64::INFINITY),
                ),
                ("a_invalid".to_string(), protocol::Value::Float(f64::NAN)),
            ]);
            let error = protocol_to_core_value_map(&invalid)
                .expect_err("both protocol values are non-finite");
            assert_eq!(
                error.to_string(),
                "map key `a_invalid`",
                "iteration {iteration}"
            );

            let valid = protocol::Value::Map(BTreeMap::from([
                ("z_last".to_string(), protocol::Value::Int(2)),
                ("a_first".to_string(), protocol::Value::Int(1)),
            ]));
            let Value::Concrete(ConcreteValue::Map(decoded)) =
                protocol_to_core_value(&valid).expect("finite protocol map must decode")
            else {
                panic!("protocol map must decode to a concrete core map")
            };
            assert_eq!(
                decoded.keys().map(String::as_str).collect::<Vec<_>>(),
                ["a_first", "z_last"],
                "iteration {iteration}"
            );
        }
    }

    #[test]
    fn schema_default_projection_rejects_non_wire_values_and_every_non_finite_float() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                try_core_to_protocol_value(&Value::Concrete(ConcreteValue::Float(value))),
                Err(ProtocolConvertError::NonFiniteFloat)
            ));
            assert!(matches!(
                protocol_schema_default_to_core(&protocol::Value::Float(value)),
                Err(ProtocolConvertError::NonFiniteFloat)
            ));
        }
        assert!(matches!(
            try_core_to_protocol_value(&Value::Concrete(ConcreteValue::Duration(
                std::time::Duration::from_secs(u64::MAX),
            ))),
            Err(ProtocolConvertError::DurationOutOfRange)
        ));

        let resolved_function = std::sync::Arc::new(crate::parser::UserFunction {
            name: "fixture".to_string(),
            params: Vec::new(),
            return_type: None,
            local_lets: Vec::new(),
            body: crate::parser::UserFunctionBody {
                value: Value::Concrete(ConcreteValue::Int(1)),
                source_line: 0,
                source_file: None,
            },
        });
        let access = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamAllExports {
                    binding: "upstream".to_string(),
                },
            )),
            crate::resource::PathSegment::Field {
                name: "field".to_string(),
            },
            0,
            crate::resource::AccessMissPolicy::Defer,
        )
        .expect("unknown field access must remain deferred");
        let digest = format!("argon2-legacy:73616c74:{}", "ab".repeat(32));
        let hash =
            RedactedSecretHash::from_encoded_digest(&digest).expect("fixture digest must be valid");
        let non_wire = [
            Value::Concrete(ConcreteValue::enum_identifier("test.Mode.enabled")),
            Value::resource_ref("source", "value", Vec::new()),
            Value::Deferred(DeferredValue::BindingRef {
                binding: "source".to_string(),
            }),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::Deferred(DeferredValue::Unknown(
                    crate::resource::ApplyTimeUnknown::UpstreamAllExports {
                        binding: "upstream".to_string(),
                    },
                ))),
                default: Box::new(Value::Concrete(ConcreteValue::Int(1))),
            }),
            Value::Deferred(DeferredValue::Interpolation(Vec::new())),
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: Vec::new(),
                source_line: (0).into(),
            }),
            Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::upper,
                args: Vec::new(),
                reason: crate::resource::UnavailableReason::from_builtin_error(
                    "fixture failure".to_string(),
                ),
                source_line: (0).into(),
            }),
            Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: resolved_function,
                functions: std::sync::Arc::new(HashMap::new()),
                args: Vec::new(),
                source_line: (0).into(),
            }),
            access,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("secret".into()),
            )))),
            Value::Deferred(DeferredValue::RedactedSecret { hash }),
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamAllExports {
                    binding: "upstream".to_string(),
                },
            )),
        ];

        for value in non_wire {
            assert!(matches!(
                try_core_to_protocol_value(&value),
                Err(ProtocolConvertError::NonWireValue { variant: _variant })
            ));
        }
    }

    #[test]
    fn closed_enum_rejects_wire_loss_for_transform_and_non_string_base() {
        let identity = TypeIdentity::new(Some("test"), ["widget"], "Mode");
        let with_transform = AttributeType::enum_(
            identity.clone(),
            Some(vec!["api-value".to_string()]),
            Vec::new(),
            None,
            Some(crate::schema::DslTransform::HyphenToUnderscore),
        );
        assert!(matches!(
            attribute_type_to_protocol(&with_transform),
            Err(ProtocolConvertError::ClosedEnumWithTransform { identity: actual })
                if actual == identity
        ));

        let with_int_base = AttributeType::enum_with_base(
            identity.clone(),
            AttributeType::int(),
            Some(vec!["1".to_string()]),
            Vec::new(),
            None,
            None,
        );
        assert!(matches!(
            attribute_type_to_protocol(&with_int_base),
            Err(ProtocolConvertError::ClosedEnumUnsupportedBase {
                identity: actual,
                base_kind,
            }) if actual == identity && base_kind == "Int"
        ));
    }

    #[test]
    fn protocol_enums_never_inherit_a_scalar_builtin_validator() {
        let wire = protocol::AttributeType::StringEnum {
            values: vec!["1".to_string()],
            name: "PositiveInt".to_string(),
            namespace: None,
            dsl_aliases: Vec::new(),
        };
        let decoded = protocol_to_attribute_type(&wire).expect("enum must decode");
        let RawShape::Enum { validate, .. } = decoded.raw_shape() else {
            panic!("StringEnum must decode as a core enum")
        };
        assert!(
            validate.is_none(),
            "an Int builtin identity must not attach its callback to an enum"
        );
    }

    #[test]
    fn contextual_errors_display_only_their_context_and_expose_the_source() {
        let error = ProtocolConvertError::NonFiniteFloat.context("list index 0");
        assert_eq!(error.to_string(), "list index 0");
        assert_eq!(
            std::error::Error::source(&error)
                .expect("context must expose its source")
                .to_string(),
            "non-finite float cannot cross the provider protocol"
        );

        let decode = SchemaDecodeError::from_default(error);
        assert_eq!(decode.to_string(), "schema default value conversion error");
        assert!(std::error::Error::source(&decode).is_some());
    }

    #[test]
    fn schema_validation_issue_is_rendered_once_across_the_error_chain() {
        fn chain(error: &SchemaDecodeError) -> String {
            let mut rendered = vec![error.to_string()];
            let mut source = std::error::Error::source(error);
            while let Some(current) = source {
                rendered.push(current.to_string());
                source = current.source();
            }
            rendered.join(" -> ")
        }

        let config_validation = ValidatedConfigSchema::validate(BTreeMap::from([(
            "value".to_string(),
            AttributeType::ref_("Missing"),
        )]))
        .expect_err("the fixture must fail schema validation");
        let config = SchemaDecodeError::config_schema_validation(config_validation);
        let resource_validation = ResourceSchema::new("test.Widget")
            .attribute(AttributeSchema::optional("value", AttributeType::ref_("Missing")).build())
            .validate()
            .expect_err("the resource fixture must fail schema validation");
        let resource =
            SchemaDecodeError::resource_schema_validation("test.Widget", resource_validation);

        for error in [&config, &resource] {
            let rendered = chain(error);
            assert_eq!(
                rendered.matches("Missing").count(),
                1,
                "the issue belongs to the source, not both context and source: {rendered}"
            );
        }
    }

    #[test]
    fn validated_schema_round_trips_through_real_json_projection() {
        let schema = ResourceSchema::new("test.Widget")
            .attribute(
                AttributeSchema::optional("aliases", AttributeType::list(AttributeType::string()))
                    .with_default(Value::Concrete(ConcreteValue::StringList(vec![
                        "one".into(),
                        "two".into(),
                    ])))
                    .with_completions(vec![CompletionValue::new("one", "first alias")])
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::list(AttributeType::struct_(
                        "Setting",
                        vec![
                            StructField::required("key", AttributeType::string()).build(),
                            StructField::required("value", AttributeType::string()).build(),
                        ],
                    )),
                )
                .keyed_partial_update("key")
                .build(),
            )
            .with_declared_validators(vec![ValidatorType::TagsKeyValueCheck])
            .with_default_wait_timeout(std::time::Duration::from_secs(30))
            .with_default_wait_interval(std::time::Duration::from_secs(2))
            .validate()
            .expect("portable fixture schema must validate");

        let invalid_tags = HashMap::from([(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("Name".into())),
                ),
                (
                    "value".to_string(),
                    Value::Concrete(ConcreteValue::String("widget".into())),
                ),
            ]))),
        )]);
        assert!(
            schema.validate(&invalid_tags).is_err(),
            "declared tags validation must execute after direct core construction"
        );

        let wire = resource_schema_to_protocol(&schema).expect("schema must encode");
        let json = serde_json::to_string(&wire).expect("protocol schema must serialize");
        let decoded_wire: protocol::ResourceSchema =
            serde_json::from_str(&json).expect("protocol schema JSON must decode");
        let decoded = protocol_to_resource_schema(&decoded_wire)
            .expect("JSON-decoded schema must regain a validation witness");

        let aliases = decoded
            .parts()
            .attributes
            .get("aliases")
            .expect("aliases attribute");
        assert!(matches!(
            aliases.default_value(),
            Some(Value::Concrete(ConcreteValue::List(values))) if values.len() == 2
        ));
        assert!(aliases.completions().is_none());
        assert_eq!(decoded.parts().default_wait_timeout, None);
        assert_eq!(decoded.parts().default_wait_interval, None);
        assert!(matches!(
            decoded.parts().validation,
            ResourceValidation::Declared(validators)
                if validators == &[ValidatorType::TagsKeyValueCheck]
        ));
        assert_eq!(
            decoded
                .effective_attribute("settings")
                .expect("settings attribute")
                .keyed_partial_update()
                .expect("keyed partial-update metadata")
                .key_field()
                .as_str(),
            "key"
        );

        assert!(
            decoded.validate(&invalid_tags).is_err(),
            "declared tags validation must execute after JSON decode"
        );
    }

    #[test]
    fn protocol_decode_rejects_invalid_keyed_partial_update_through_schema_validation() {
        let schema = ResourceSchema::new("test.Settings")
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::list(AttributeType::struct_(
                        "Setting",
                        vec![StructField::required("key", AttributeType::string()).build()],
                    )),
                )
                .keyed_partial_update("key")
                .build(),
            )
            .validate()
            .expect("fixture schema validates");
        let mut wire = resource_schema_to_protocol(&schema).expect("schema encodes");
        wire.attributes
            .get_mut("settings")
            .expect("settings wire attribute")
            .keyed_partial_update = Some("missing".to_string());

        let error = protocol_to_resource_schema(&wire)
            .expect_err("wire key must be checked by whole-schema validation");
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<ResourceSchemaValidationError>())
            .expect("typed resource-schema validation error remains in the source chain");
        assert!(source.issues().any(|(path, message)| {
            path == "settings"
                && message
                    == "keyed partial-update key `missing` is not a field of list element struct `Setting`"
        }));
    }

    #[test]
    fn field_groups_round_trip_at_resource_and_struct_scope() {
        let nested_group = FieldGroup::at_least_one(["left", "right"]).unwrap();
        let top_group = FieldGroup::at_most_one(["first", "second"]).unwrap();
        let schema = ResourceSchema::new("test.Groups")
            .attribute(AttributeSchema::optional("first", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("second", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    "nested",
                    AttributeType::struct_with_field_groups(
                        "Nested",
                        vec![
                            StructField::optional("left", AttributeType::string()).build(),
                            StructField::optional("right", AttributeType::string()).build(),
                        ],
                        vec![nested_group.clone()],
                    ),
                )
                .build(),
            )
            .field_group(top_group.clone())
            .validate()
            .unwrap();

        let wire = resource_schema_to_protocol(&schema).unwrap();
        assert_eq!(wire.field_groups.len(), 1);
        let protocol::AttributeType::Struct { field_groups, .. } =
            &wire.attributes["nested"].attr_type
        else {
            panic!("nested attribute must remain a struct");
        };
        assert_eq!(field_groups.len(), 1);

        let json = serde_json::to_string(&wire).unwrap();
        let decoded_wire = serde_json::from_str(&json).unwrap();
        let decoded = protocol_to_resource_schema(&decoded_wire).unwrap();
        assert_eq!(decoded.parts().field_groups, &[top_group]);
        let RawShape::Struct { field_groups, .. } =
            decoded.parts().attributes["nested"].attr_type().raw_shape()
        else {
            panic!("decoded nested attribute must remain a struct");
        };
        assert_eq!(field_groups, &[nested_group]);
    }

    #[test]
    fn protocol_decode_rejects_field_groups_with_fewer_than_two_fields() {
        let schema = ResourceSchema::new("test.Groups")
            .attribute(AttributeSchema::optional("only", AttributeType::string()).build())
            .validate()
            .unwrap();
        let mut wire = resource_schema_to_protocol(&schema).unwrap();
        for fields in [vec![], vec!["only".to_string()]] {
            wire.field_groups = vec![protocol::FieldGroup {
                kind: protocol::FieldGroupKind::ExactlyOne,
                fields,
            }];
            let error = protocol_to_resource_schema(&wire)
                .expect_err("invalid wire group must fail schema admission");
            let source = std::error::Error::source(&error)
                .expect("field-group construction error must be retained")
                .to_string();
            assert!(source.contains("at least two fields"), "{source}");
        }

        let mut wire = resource_schema_to_protocol(&schema).unwrap();
        wire.attributes.get_mut("only").unwrap().attr_type = protocol::AttributeType::Struct {
            name: "Nested".to_string(),
            fields: vec![protocol::StructField {
                name: "nested_only".to_string(),
                field_type: protocol::AttributeType::String {
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                    identity: None,
                },
                required: false,
                description: None,
                block_name: None,
                provider_name: None,
                read_only: false,
                write_only: false,
                deferred_populate: false,
            }],
            field_groups: vec![protocol::FieldGroup {
                kind: protocol::FieldGroupKind::AtLeastOne,
                fields: vec!["nested_only".to_string()],
            }],
        };
        let error = protocol_to_resource_schema(&wire)
            .expect_err("invalid nested wire group must fail schema admission");
        let rendered = error.to_string();
        assert!(
            rendered.contains("attribute 'only.field_groups[0]'"),
            "{rendered}"
        );
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .to_string()
                .contains("at least two fields")
        );
    }

    #[test]
    fn json_decode_reconstructs_every_registered_builtin_validator() {
        let positive_int = TypeIdentity::bare("PositiveInt");
        for (kind, identity) in crate::schema::builtin_validator_registry().keys() {
            let wire = match kind {
                crate::schema::RefinedScalarKind::String => protocol::AttributeType::String {
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                    identity: Some(identity.to_string()),
                },
                crate::schema::RefinedScalarKind::Int => protocol::AttributeType::Int {
                    range: None,
                    identity: Some(identity.to_string()),
                },
                crate::schema::RefinedScalarKind::Float => protocol::AttributeType::Float {
                    range: None,
                    identity: Some(identity.to_string()),
                },
            };
            let json = serde_json::to_string(&wire).unwrap();
            let decoded_wire: protocol::AttributeType = serde_json::from_str(&json).unwrap();
            let decoded = protocol_to_attribute_type(&decoded_wire).unwrap();
            let (decoded_identity, has_callback) = match decoded.raw_shape() {
                RawShape::String {
                    identity,
                    pattern: _pattern,
                    length: _length,
                    validate,
                    to_dsl: _to_dsl,
                } => (identity, validate.is_some()),
                RawShape::Int {
                    identity,
                    range: _range,
                    validate,
                } => (identity, validate.is_some()),
                RawShape::Float {
                    identity,
                    range: _range,
                    validate,
                } => (identity, validate.is_some()),
                RawShape::Bool => panic!("unexpected Bool builtin wire shape for {identity}"),
                RawShape::Duration => {
                    panic!("unexpected Duration builtin wire shape for {identity}")
                }
                RawShape::Enum {
                    identity: _enum_identity,
                    base: _base,
                    values: _values,
                    dsl_aliases: _dsl_aliases,
                    validate: _validate,
                    to_dsl: _to_dsl,
                } => panic!("unexpected Enum builtin wire shape for {identity}"),
                RawShape::List {
                    element_type: _element_type,
                    ordered: _ordered,
                    length: _length,
                } => panic!("unexpected List builtin wire shape for {identity}"),
                RawShape::Map {
                    key: _key,
                    value: _value,
                } => panic!("unexpected Map builtin wire shape for {identity}"),
                RawShape::Struct {
                    name: _name,
                    fields: _fields,
                    field_groups: _field_groups,
                } => panic!("unexpected Struct builtin wire shape for {identity}"),
                RawShape::Union(_members) => {
                    panic!("unexpected Union builtin wire shape for {identity}")
                }
                RawShape::Ref(_name) => {
                    panic!("unexpected Ref builtin wire shape for {identity}")
                }
            };
            assert_eq!(decoded_identity, Some(identity));
            assert!(
                has_callback,
                "builtin callback was not restored for {identity}"
            );
        }

        let wire_schema = protocol::ResourceSchema {
            resource_type: "test.Counter".to_string(),
            attributes: BTreeMap::from([(
                "count".to_string(),
                protocol::AttributeSchema {
                    name: "count".to_string(),
                    attr_type: protocol::AttributeType::Int {
                        range: None,
                        identity: Some(positive_int.to_string()),
                    },
                    required: false,
                    keyed_partial_update: None,
                    default: None,
                    description: None,
                    create_only: false,
                    read_only: false,
                    write_only: false,
                    block_name: None,
                    provider_name: None,
                    removable: None,
                    identity: false,
                    deferred_populate: false,
                },
            )]),
            description: None,
            kind: protocol::SchemaKind::Managed,
            unique_name: protocol::UniqueNameSpec::Conflicting,
            operation_config: None,
            validators: Vec::new(),
            field_groups: Vec::new(),
            defs: Default::default(),
        };
        let json = serde_json::to_string(&wire_schema).unwrap();
        let decoded_wire: protocol::ResourceSchema = serde_json::from_str(&json).unwrap();
        let decoded = protocol_to_resource_schema(&decoded_wire).unwrap();
        let attributes = |count| {
            HashMap::from([(
                "count".to_string(),
                Value::Concrete(ConcreteValue::Int(count)),
            )])
        };
        assert!(decoded.validate(&attributes(1)).is_ok());
        assert!(decoded.validate(&attributes(0)).is_err());
    }

    #[test]
    fn v13_json_decode_rejects_an_empty_custom_enum_namespace() {
        let wire = protocol::AttributeType::CustomEnum {
            name: "Mode".to_string(),
            base: Box::new(protocol::AttributeType::String {
                pattern: None,
                length: None,
                validate: None,
                to_dsl: None,
                identity: None,
            }),
            namespace: String::new(),
            dsl_transform: None,
        };
        let json = serde_json::to_string(&wire).expect("v13 CustomEnum must serialize");
        let decoded_wire: protocol::AttributeType =
            serde_json::from_str(&json).expect("v13 CustomEnum JSON must decode");

        let error = protocol_to_attribute_type(&decoded_wire)
            .expect_err("an empty namespace cannot identify a provider custom enum");
        let source = std::error::Error::source(&error).expect("typed decode cause");
        assert!(matches!(
            source.downcast_ref::<ProtocolConvertError>(),
            Some(ProtocolConvertError::EmptyCustomEnumNamespace { name }) if name == "Mode"
        ));
        assert!(error.to_string().contains("namespace"), "{error}");
    }

    #[test]
    fn legacy_custom_string_preserves_base_constraints() {
        let wire = protocol::AttributeType::Custom {
            name: "test.StringId".to_string(),
            base: Box::new(protocol::AttributeType::String {
                pattern: Some("^base-[a-z]+$".to_string()),
                length: Some((Some(5), Some(32))),
                validate: None,
                to_dsl: Some(protocol::DslTransform::HyphenToUnderscore),
                identity: None,
            }),
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
        };

        let decoded = protocol_to_attribute_type(&wire).expect("base metadata is representable");
        assert!(matches!(
            decoded.raw_shape(),
            RawShape::String {
                identity: Some(identity),
                pattern: Some(pattern),
                length: Some((Some(5), Some(32))),
                validate: None,
                to_dsl: Some(protocol::DslTransform::HyphenToUnderscore),
            } if identity == &TypeIdentity::from_dotted("test.StringId")
                && pattern == "^base-[a-z]+$"
        ));
    }

    #[test]
    fn anonymous_legacy_custom_string_does_not_inherit_its_base_identity() {
        let wire = protocol::AttributeType::Custom {
            name: String::new(),
            base: Box::new(protocol::AttributeType::String {
                pattern: None,
                length: None,
                validate: None,
                to_dsl: None,
                identity: Some("base.Identity".to_string()),
            }),
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
        };

        let decoded = protocol_to_attribute_type(&wire)
            .expect("an anonymous legacy Custom string remains representable");
        assert!(matches!(
            decoded.raw_shape(),
            RawShape::String { identity: None, .. }
        ));
    }

    #[test]
    fn anonymous_legacy_custom_does_not_inherit_a_nested_custom_identity() {
        let wire = protocol::AttributeType::Custom {
            name: String::new(),
            base: Box::new(protocol::AttributeType::Custom {
                name: "nested.Identity".to_string(),
                base: Box::new(protocol::AttributeType::String {
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                    identity: None,
                }),
                pattern: None,
                length: None,
                validate: None,
                to_dsl: None,
            }),
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
        };

        let decoded = protocol_to_attribute_type(&wire)
            .expect("a nested reusable Custom does not supply the outer identity");
        assert!(matches!(
            decoded.raw_shape(),
            RawShape::String { identity: None, .. }
        ));
    }

    #[test]
    fn anonymous_legacy_custom_list_remains_a_refined_list() {
        let wire = protocol::AttributeType::Custom {
            name: String::new(),
            base: Box::new(protocol::AttributeType::List {
                element_type: Box::new(protocol::AttributeType::String {
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                    identity: None,
                }),
                ordered: false,
                length: Some((Some(2), Some(4))),
                validate: None,
            }),
            pattern: None,
            length: Some((Some(1), Some(5))),
            validate: None,
            to_dsl: None,
        };

        let decoded = protocol_to_attribute_type(&wire)
            .expect("an anonymous legacy Custom list has no lost identity");
        assert!(matches!(
            decoded.raw_shape(),
            RawShape::List {
                ordered: false,
                length: Some((Some(1), Some(5))),
                ..
            }
        ));
    }

    #[test]
    fn legacy_custom_numeric_types_preserve_base_ranges() {
        let int_wire = protocol::AttributeType::Custom {
            name: "test.Port".to_string(),
            base: Box::new(protocol::AttributeType::Int {
                range: Some((Some(1), Some(65_535))),
                identity: None,
            }),
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
        };
        let float_wire = protocol::AttributeType::Custom {
            name: "test.Ratio".to_string(),
            base: Box::new(protocol::AttributeType::Float {
                range: Some((Some(0.0), Some(1.0))),
                identity: None,
            }),
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
        };

        let int_decoded =
            protocol_to_attribute_type(&int_wire).expect("integer range is representable");
        assert!(matches!(
            int_decoded.raw_shape(),
            RawShape::Int {
                range: Some((Some(1), Some(65_535))),
                ..
            }
        ));
        let float_decoded =
            protocol_to_attribute_type(&float_wire).expect("float range is representable");
        assert!(matches!(
            float_decoded.raw_shape(),
            RawShape::Float {
                range: Some((Some(min), Some(max))),
                ..
            } if min == 0.0 && max == 1.0
        ));
    }

    #[test]
    fn named_legacy_custom_list_rejects_unrepresentable_identity_and_prefers_outer_constraint() {
        let list_wire = protocol::AttributeType::Custom {
            name: "test.IdentifiedList".to_string(),
            base: Box::new(protocol::AttributeType::List {
                element_type: Box::new(protocol::AttributeType::String {
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                    identity: None,
                }),
                ordered: true,
                length: None,
                validate: None,
            }),
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
        };
        let duplicate_wire = protocol::AttributeType::Custom {
            name: "test.ConflictingString".to_string(),
            base: Box::new(protocol::AttributeType::String {
                pattern: Some("^base$".to_string()),
                length: None,
                validate: None,
                to_dsl: None,
                identity: None,
            }),
            pattern: Some("^outer$".to_string()),
            length: None,
            validate: None,
            to_dsl: None,
        };

        let list_error = protocol_to_attribute_type(&list_wire)
            .expect_err("core has no identified-list representation");
        assert!(matches!(
            std::error::Error::source(&list_error)
                .and_then(|source| source.downcast_ref::<ProtocolConvertError>()),
            Some(ProtocolConvertError::LegacyCustomUnsupportedBase {
                identity,
                base_kind: "List",
            }) if identity == "test.IdentifiedList"
        ));
        let decoded = protocol_to_attribute_type(&duplicate_wire)
            .expect("v13 defines the enclosing Custom refinement as authoritative");
        assert!(matches!(
            decoded.raw_shape(),
            RawShape::String {
                pattern: Some("^outer$"),
                ..
            }
        ));
    }

    #[test]
    fn update_patch_round_trip_preserves_order_and_kinds() {
        let patch = UpdatePatch {
            ops: vec![
                PatchOp::Add {
                    key: "first".to_string(),
                    value: Value::Concrete(ConcreteValue::Int(1)),
                },
                PatchOp::Reassert {
                    key: "secret".to_string(),
                    value: Value::Concrete(ConcreteValue::String("value".into())),
                },
                PatchOp::Remove {
                    key: "old".to_string(),
                },
            ],
        };

        let wire = try_update_patch_to_protocol(&patch).expect("patch must encode");
        let decoded = protocol_update_patch_to_core(&wire).expect("patch must decode");
        assert_eq!(decoded.ops, patch.ops);
    }
}
