//! The shared state-document decoder used by state loading and observation authority.
use crate::explicit::ExplicitFields;
use crate::name_override::NameOverride;
use crate::provider::{
    AppliedIdentifier, InFlightCreate, NestedWriteOnlyRecord, PersistedAttributes,
    StateRowInFlightCreate, StateRowUnknownCreateOutcome, UnknownCreateOutcome,
};
use crate::resource::{
    AbsoluteBindingAddress, DeposedKey, PartialReadMarker, PersistedDestroyDependencies,
    PersistedDirectives, ResolvedResourceId, ResourceId, ResourceIdentity, ResourceIdentityError,
};
use crate::value::PersistedExportValue;
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub const CURRENT_STATE_VERSION: u32 = 21;
pub const CURRENT_EXPORTS_VERSION: u32 = 1;
pub const EXPORTS_VERSION_INTRODUCED: u32 = 17;

/// Version gate shared by the full decoder and the state's legacy migrations.
#[derive(Debug, Deserialize)]
pub struct StateDocumentVersion {
    pub version: u32,
    pub exports_version: Option<u32>,
}
impl StateDocumentVersion {
    pub fn read(bytes: &[u8]) -> Result<Self, StateDocumentDecodeError> {
        let header: Self =
            serde_json::from_slice(bytes).map_err(StateDocumentDecodeError::json_at_root)?;
        if header.version > CURRENT_STATE_VERSION {
            return Err(StateDocumentDecodeError::StateVersionTooNew {
                found: header.version,
                supported: CURRENT_STATE_VERSION,
            });
        }
        classify_exports_version(header.version, header.exports_version)?;
        Ok(header)
    }
}

/// A complete, typed document. Observation authority can only be minted by
/// decoding bytes, never by passing caller-built parts back to the owner.
#[derive(Debug, Clone)]
pub struct DecodedStateDocument {
    pub version: u32,
    pub serial: u64,
    pub lineage: String,
    pub carina_version: String,
    pub resources: Vec<DecodedResourceState>,
    pub exports: HashMap<String, PersistedExportValue>,
    pub legacy_dependencies: BTreeMap<ResolvedResourceId, LegacyRowDependencies>,
    pub wire_order: std::sync::Arc<crate::json_field_order::JsonFieldOrder>,
}

#[derive(Deserialize)]
struct UncheckedStateFile {
    version: u32,
    serial: u64,
    lineage: String,
    carina_version: String,
    resources: Vec<serde_json::Value>,
    #[serde(default)]
    exports: HashMap<String, serde_json::Value>,
}

struct ErrorSourceChain<'a>(&'a (dyn std::error::Error + 'static));

impl std::fmt::Display for ErrorSourceChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(f, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum StateDocumentDecodeError {
    Json {
        json_path: String,
        source: serde_json::Error,
    },
    ExportsVersionTooNew {
        found: u32,
        supported: u32,
    },
    StateVersionTooNew {
        found: u32,
        supported: u32,
    },
    InvalidResourceIdentity {
        resource_index: usize,
        provider: String,
        resource_type: String,
        identifier: Option<String>,
        source: ResourceIdentityError,
    },
    Validation(String),
}

impl StateDocumentDecodeError {
    pub fn json_at_root(source: serde_json::Error) -> Self {
        Self::Json {
            json_path: "<root>".to_string(),
            source,
        }
    }

    pub fn json_at_path(path: &serde_path_to_error::Path, source: serde_json::Error) -> Self {
        let path = path.to_string();
        Self::Json {
            json_path: if path == "." {
                "<root>".to_string()
            } else {
                path
            },
            source,
        }
    }

    pub fn json_in_resource(
        index: usize,
        path: &serde_path_to_error::Path,
        source: serde_json::Error,
    ) -> Self {
        let path = path.to_string();
        let json_path = if path == "." {
            format!("resources[{index}]")
        } else if path.starts_with('[') {
            format!("resources[{index}]{path}")
        } else {
            format!("resources[{index}].{path}")
        };
        Self::Json { json_path, source }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportsVersionClassification {
    Current,
    Legacy,
}

pub fn classify_exports_version(
    state_version: u32,
    exports_version: Option<u32>,
) -> Result<ExportsVersionClassification, StateDocumentDecodeError> {
    match exports_version {
        Some(_) if state_version < EXPORTS_VERSION_INTRODUCED => {
            Err(StateDocumentDecodeError::Validation(format!(
                "top-level exports_version is invalid before state version {}",
                EXPORTS_VERSION_INTRODUCED
            )))
        }
        Some(found) if found > CURRENT_EXPORTS_VERSION => {
            Err(StateDocumentDecodeError::ExportsVersionTooNew {
                found,
                supported: CURRENT_EXPORTS_VERSION,
            })
        }
        Some(found) if found < CURRENT_EXPORTS_VERSION => {
            Err(StateDocumentDecodeError::Validation(format!(
                "exports version {found} is older than supported version {}; no exports migration exists",
                CURRENT_EXPORTS_VERSION
            )))
        }
        Some(_) => Ok(ExportsVersionClassification::Current),
        None if state_version >= EXPORTS_VERSION_INTRODUCED => {
            Err(StateDocumentDecodeError::Validation(
                "missing required top-level exports_version".to_string(),
            ))
        }
        None => Ok(ExportsVersionClassification::Legacy),
    }
}

/// Validate persisted JSON without applying remote-binding value semantics.
pub fn decode_persisted_exports(
    exports: HashMap<String, serde_json::Value>,
) -> Result<HashMap<String, PersistedExportValue>, StateDocumentDecodeError> {
    exports
        .into_iter()
        .map(|(name, value)| {
            serde_json::from_value(value)
                .map(|value| (name.clone(), value))
                .map_err(|error| {
                    StateDocumentDecodeError::Validation(format!(
                        "persisted export {name:?} is not valid persisted JSON: {error}"
                    ))
                })
        })
        .collect()
}

/// Binding texts retained from the kind-blind dependency snapshot written by
/// state v20 and older.
///
/// Arbitrary name sets cannot be promoted through a public conversion.
///
/// ```compile_fail,E0277
/// use carina_core::state_document::LegacyDependencyNames;
/// use std::collections::BTreeSet;
/// let names: LegacyDependencyNames = BTreeSet::from([String::from("target")]).into();
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyDependencyNames(BTreeSet<String>);

impl LegacyDependencyNames {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyRowDependencies {
    pub value_dependencies: LegacyDependencyNames,
    pub explicit_dependencies: LegacyDependencyNames,
    pub deposed_dependencies: HashMap<DeposedKey, LegacyDependencyNames>,
}

impl LegacyRowDependencies {
    pub fn is_empty(&self) -> bool {
        self.value_dependencies.is_empty()
            && self.explicit_dependencies.is_empty()
            && self
                .deposed_dependencies
                .values()
                .all(LegacyDependencyNames::is_empty)
    }

    pub fn all_names(&self) -> LegacyDependencyNames {
        LegacyDependencyNames(
            self.value_dependencies
                .0
                .iter()
                .chain(self.explicit_dependencies.0.iter())
                .chain(
                    self.deposed_dependencies
                        .values()
                        .flat_map(|names| names.0.iter()),
                )
                .cloned()
                .collect(),
        )
    }
}

fn take_legacy_dependency_names(
    owner: &mut serde_json::Value,
    field: &str,
    location: &str,
) -> Result<LegacyDependencyNames, StateDocumentDecodeError> {
    use crate::utils::canonicalize_map_key_address;

    let Some(object) = owner.as_object_mut() else {
        return Err(StateDocumentDecodeError::Validation(format!(
            "{location} owner must be an object"
        )));
    };
    let Some(value) = object.remove(field) else {
        return Ok(LegacyDependencyNames::default());
    };
    let values = value.as_array().ok_or_else(|| {
        StateDocumentDecodeError::Validation(format!("{location} must be an array"))
    })?;
    let names = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(canonicalize_map_key_address)
                .ok_or_else(|| {
                    StateDocumentDecodeError::Validation(format!(
                        "{location}[{index}] must be a string in pre-v21 state"
                    ))
                })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(LegacyDependencyNames(names))
}

fn extract_legacy_row_dependencies(
    row: &mut serde_json::Value,
    row_index: usize,
) -> Result<LegacyRowDependencies, StateDocumentDecodeError> {
    let value_dependencies = take_legacy_dependency_names(
        row,
        "dependency_bindings",
        &format!("resources[{row_index}].dependency_bindings"),
    )?;
    let explicit_dependencies = match row.get_mut("directives") {
        Some(directives) => take_legacy_dependency_names(
            directives,
            "depends_on",
            &format!("resources[{row_index}].directives.depends_on"),
        )?,
        None => LegacyDependencyNames::default(),
    };
    let mut deposed_dependencies = HashMap::new();
    if let Some(deposed) = row.get_mut("deposed") {
        let deposed = deposed.as_array_mut().ok_or_else(|| {
            StateDocumentDecodeError::Validation(format!(
                "resources[{row_index}].deposed must be an array"
            ))
        })?;
        for (generation_index, generation) in deposed.iter_mut().enumerate() {
            let names = take_legacy_dependency_names(
                generation,
                "dependency_bindings",
                &format!("resources[{row_index}].deposed[{generation_index}].dependency_bindings"),
            )?;
            if !names.is_empty() {
                let key = generation
                    .get("key")
                    .cloned()
                    .ok_or_else(|| {
                        StateDocumentDecodeError::Validation(format!(
                            "resources[{row_index}].deposed[{generation_index}].key is missing"
                        ))
                    })
                    .and_then(|value| {
                        serde_json::from_value::<DeposedKey>(value)
                            .map_err(StateDocumentDecodeError::json_at_root)
                    })?;
                deposed_dependencies.insert(key, names);
            }
        }
    }
    Ok(LegacyRowDependencies {
        value_dependencies,
        explicit_dependencies,
        deposed_dependencies,
    })
}

/// The mutually exclusive state of a row's current resource instance.
///
/// The tagged shape is the persistence invariant for state v15: an applied
/// identifier and provider continuation token cannot be represented together.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CurrentInstance {
    /// The row retains only deposed generations.
    None,
    /// A completed resource instance.
    Applied {
        identifier: AppliedIdentifier,
        attributes: PersistedAttributes,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_read: Option<PartialReadMarker>,
    },
    /// A create operation accepted by the provider but not yet ended.
    CreateInFlight {
        #[serde(flatten)]
        pending: InFlightCreate,
    },
    /// A create may have reached the cloud, but yielded neither an identifier
    /// nor a resumable continuation.
    CreateOutcomeUnknown {
        #[serde(flatten)]
        unknown: UnknownCreateOutcome,
    },
    /// A legacy current instance whose provider identifier is absent.
    MissingIdentifier {
        attributes: PersistedAttributes,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_read: Option<PartialReadMarker>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EncodedCurrentInstance {
    None,
    Applied {
        identifier: AppliedIdentifier,
        attributes: HashMap<String, serde_json::Value>,
        #[serde(default)]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default)]
        partial_read: Option<PartialReadMarker>,
    },
    CreateInFlight {
        envelope: serde_json::Value,
        started_at: serde_json::Value,
    },
    CreateOutcomeUnknown {
        started_at: serde_json::Value,
    },
    MissingIdentifier {
        attributes: HashMap<String, serde_json::Value>,
        #[serde(default)]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default)]
        partial_read: Option<PartialReadMarker>,
    },
}

#[derive(Debug, Clone)]
pub struct DecodedResourceState {
    /// Resource type (e.g., "s3.Bucket", "ec2.Vpc")
    pub resource_type: String,
    /// Resource identity (from the DSL resource address identity)
    pub identity: ResourceIdentity,
    /// Provider name (e.g., "aws")
    pub provider: String,
    /// The current resource instance, if any.
    pub current: CurrentInstance,
    /// Old replacement generations that still need a successful Delete.
    pub deposed: Vec<DecodedDeposedInstance>,
    /// Whether this resource is protected from deletion (e.g., state bucket)
    pub protected: bool,
    /// Carina-side directives persisted from the DSL `directives` block.
    pub directives: PersistedDirectives,
    /// Attribute prefixes used to generate names (e.g., {"bucket_name": "my-app-"})
    pub prefixes: HashMap<String, String>,
    /// Permanent name overrides from create_before_destroy with non-renameable attributes.
    /// Maps attribute name to the temporary cloud-side name and the DSL value recorded with it.
    pub name_overrides: HashMap<String, NameOverride>,
    /// Tree of fields the user explicitly wrote in their `.crn` for
    /// this resource. Used by the differ both to detect attribute
    /// removals and to project actual-state through the authoring
    /// shape so server-side default fields the user never wrote stop
    /// appearing as spurious removals (refs awscc#206).
    ///
    /// Replaces the flat `desired_keys: Vec<String>` (state ≤ v5);
    /// the v5 reader lifts each top-level key to a `Leaf` child of
    /// the root `Struct`. Later writes rebuild a full tree from the resource's
    /// authored `Value` via an evidence-specific aligned builder.
    pub explicit: ExplicitFields,
    /// The binding name for this resource (from `let` bindings in DSL).
    /// Stored so orphan Delete effects can have tree structure.
    pub binding: Option<AbsoluteBindingAddress>,
    /// Complete resource addresses this resource depends on (via value refs).
    /// Stored so orphan Delete effects can have tree structure.
    ///
    /// Set semantics are provided by
    /// [`crate::resource::ResolvedDependencies`]. State v20 and
    /// older string arrays are converted by the version-gated raw migration
    /// before this current representation is deserialized.
    pub dependency_bindings: PersistedDestroyDependencies,
}

#[derive(Debug, Clone)]
pub struct DecodedDeposedInstance {
    pub key: DeposedKey,
    pub identifier: String,
    pub provider_instance: Option<String>,
    pub attributes: PersistedAttributes,
    pub dependency_bindings: PersistedDestroyDependencies,
}

#[derive(Deserialize)]
struct EncodedDeposedInstance {
    key: DeposedKey,
    identifier: String,
    #[serde(default)]
    provider_instance: Option<String>,
    attributes: HashMap<String, serde_json::Value>,
    #[serde(default)]
    dependency_bindings: BTreeSet<ResolvedResourceId>,
}

#[derive(Deserialize)]
struct EncodedResourceState {
    resource_type: String,
    #[serde(alias = "name")]
    identity: ResourceIdentity,
    provider: String,
    current: EncodedCurrentInstance,
    #[serde(default)]
    deposed: Vec<EncodedDeposedInstance>,
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    directives: PersistedDirectives,
    #[serde(default)]
    prefixes: HashMap<String, String>,
    #[serde(default)]
    name_overrides: HashMap<String, NameOverride>,
    #[serde(default)]
    explicit: ExplicitFields,
    #[serde(default)]
    binding: Option<AbsoluteBindingAddress>,
    #[serde(default)]
    dependency_bindings: BTreeSet<ResolvedResourceId>,
}

impl<'de> Deserialize<'de> for DecodedResourceState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let row = serde_json::Value::deserialize(deserializer)?;
        let encoded = serde_json::from_value::<EncodedResourceState>(row.clone())
            .map_err(serde::de::Error::custom)?;
        let current = match encoded.current {
            EncodedCurrentInstance::None => CurrentInstance::None,
            EncodedCurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => CurrentInstance::Applied {
                identifier,
                attributes: PersistedAttributes::from_encoded(attributes)
                    .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?,
                write_only_attributes,
                nested_write_only,
                partial_read,
            },
            EncodedCurrentInstance::CreateInFlight {
                envelope: _envelope,
                started_at: _started_at,
            } => {
                let pending = serde_json::from_value::<StateRowInFlightCreate>(row)
                    .map_err(serde::de::Error::custom)?
                    .into_inner();
                CurrentInstance::CreateInFlight { pending }
            }
            EncodedCurrentInstance::CreateOutcomeUnknown {
                started_at: _started_at,
            } => {
                let unknown = serde_json::from_value::<StateRowUnknownCreateOutcome>(row)
                    .map_err(serde::de::Error::custom)?
                    .into_inner();
                CurrentInstance::CreateOutcomeUnknown { unknown }
            }
            EncodedCurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => CurrentInstance::MissingIdentifier {
                attributes: PersistedAttributes::from_encoded(attributes)
                    .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?,
                write_only_attributes,
                nested_write_only,
                partial_read,
            },
        };
        let deposed = encoded
            .deposed
            .into_iter()
            .enumerate()
            .map(|(index, instance)| {
                Ok(DecodedDeposedInstance {
                    key: instance.key,
                    identifier: instance.identifier,
                    provider_instance: instance.provider_instance,
                    attributes: PersistedAttributes::from_encoded(instance.attributes)
                        .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?,
                    dependency_bindings: PersistedDestroyDependencies::decode_persisted(
                        instance.dependency_bindings,
                    )
                    .map_err(|error| {
                        serde::de::Error::custom(format!(
                            "deposed[{index}].dependency_bindings: {error}"
                        ))
                    })?,
                })
            })
            .collect::<Result<Vec<_>, D::Error>>()?;
        let dependency_bindings = PersistedDestroyDependencies::decode_persisted(
            encoded.dependency_bindings,
        )
        .map_err(|error| serde::de::Error::custom(format!("dependency_bindings: {error}")))?;
        Ok(Self {
            resource_type: encoded.resource_type,
            identity: encoded.identity,
            provider: encoded.provider,
            current,
            deposed,
            protected: encoded.protected,
            directives: encoded.directives,
            prefixes: encoded.prefixes,
            name_overrides: encoded.name_overrides,
            explicit: encoded.explicit,
            binding: encoded.binding,
            dependency_bindings,
        })
    }
}

/// Borrowed identity fields shared by decoded and mutable state rows.
pub type StateRowIdentity<'a> = (&'a str, &'a str, &'a ResourceIdentity);
/// Generation key and provider identity, without copying persisted attributes.
pub type DeposedIdentity<'a> = (&'a DeposedKey, &'a str, Option<&'a str>);

pub fn validate_resource_identities<'a, D>(
    resources: impl IntoIterator<Item = (StateRowIdentity<'a>, D)>,
) -> Result<(), String>
where
    D: IntoIterator<Item = DeposedIdentity<'a>>,
{
    let mut resource_identities = HashSet::new();
    for ((provider, resource_type, identity), deposed) in resources {
        if !resource_identities.insert((provider, resource_type, identity.as_str())) {
            return Err(format!(
                "duplicate resource identity (provider={:?}, resource_type={:?}, identity={:?})",
                provider, resource_type, identity
            ));
        }
        validate_deposed_identities((provider, resource_type, identity), deposed)?;
    }
    Ok(())
}

pub fn validate_deposed_identities<'a>(
    (provider, resource_type, identity): StateRowIdentity<'_>,
    deposed: impl IntoIterator<Item = DeposedIdentity<'a>>,
) -> Result<(), String> {
    let mut keys = HashSet::new();
    let mut instance_identities = HashSet::new();
    for (key, identifier, provider_instance) in deposed {
        if !keys.insert(key.as_str()) {
            return Err(format!(
                "duplicate deposed generation key {:?} in resource \
                 (provider={:?}, resource_type={:?}, identity={:?})",
                key, provider, resource_type, identity
            ));
        }
        if !instance_identities.insert((identifier, provider_instance)) {
            return Err(format!(
                "duplicate deposed generation identity \
                 (identifier={:?}, provider_instance={:?}) in resource \
                 (provider={:?}, resource_type={:?}, identity={:?})",
                identifier, provider_instance, provider, resource_type, identity
            ));
        }
    }
    Ok(())
}

pub(crate) fn decode_state_document(
    bytes: &[u8],
) -> Result<DecodedStateDocument, StateDocumentDecodeError> {
    let content = std::str::from_utf8(bytes).map_err(|error| {
        StateDocumentDecodeError::Validation(format!("State file is not valid UTF-8: {error}"))
    })?;
    StateDocumentVersion::read(content.as_bytes())?;
    let mut deserializer = serde_json::Deserializer::from_str(content);
    let mut path = serde_path_to_error::Track::new();
    let mut wire_order = crate::json_field_order::JsonFieldOrder::default();
    let mut unchecked: UncheckedStateFile = wire_order
        .deserialize(serde_path_to_error::Deserializer::new(
            &mut deserializer,
            &mut path,
        ))
        .map_err(|error| StateDocumentDecodeError::json_at_path(&path.path(), error))?;
    deserializer
        .end()
        .map_err(StateDocumentDecodeError::json_at_root)?;
    let legacy_rows = if unchecked.version <= 20 {
        unchecked
            .resources
            .iter_mut()
            .enumerate()
            .map(|(index, row)| extract_legacy_row_dependencies(row, index))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![LegacyRowDependencies::default(); unchecked.resources.len()]
    };
    let resources = unchecked
        .resources
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let identity = value
                .get("identity")
                .or_else(|| value.get("name"))
                .and_then(serde_json::Value::as_str);
            if let Some(identity) = identity
                && let Err(source) = ResourceIdentity::try_from(identity.to_owned())
            {
                let provider = value
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<missing>")
                    .to_owned();
                let resource_type = value
                    .get("resource_type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<missing>")
                    .to_owned();
                let identifier = value
                    .get("current")
                    .and_then(|current| current.get("identifier"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                return Err(StateDocumentDecodeError::InvalidResourceIdentity {
                    resource_index: index,
                    provider,
                    resource_type,
                    identifier,
                    source,
                });
            }
            let _: EncodedResourceState = serde_path_to_error::deserialize(
                value.clone().into_deserializer(),
            )
            .map_err(|error| {
                let path = error.path().clone();
                StateDocumentDecodeError::json_in_resource(index, &path, error.into_inner())
            })?;
            serde_path_to_error::deserialize(value.into_deserializer()).map_err(|error| {
                let path = error.path().clone();
                StateDocumentDecodeError::json_in_resource(index, &path, error.into_inner())
            })
        })
        .collect::<Result<Vec<DecodedResourceState>, StateDocumentDecodeError>>()?;
    let exports = decode_persisted_exports(unchecked.exports)?;
    let legacy_dependencies = resources
        .iter()
        .zip(legacy_rows)
        .filter_map(|(row, dependencies)| {
            (!dependencies.is_empty()).then(|| {
                (
                    ResolvedResourceId::new(ResourceId::with_provider_identity(
                        &row.provider,
                        &row.resource_type,
                        row.identity.clone(),
                        row.directives.provider_instance.clone(),
                    )),
                    dependencies,
                )
            })
        })
        .collect();
    let state = DecodedStateDocument {
        version: unchecked.version,
        wire_order: std::sync::Arc::new(wire_order),
        serial: unchecked.serial,
        lineage: unchecked.lineage,
        carina_version: unchecked.carina_version,
        resources,
        exports,
        legacy_dependencies,
    };
    validate_resource_identities(state.resources.iter().map(|row| {
        (
            (
                row.provider.as_str(),
                row.resource_type.as_str(),
                &row.identity,
            ),
            row.deposed.iter().map(|old| {
                (
                    &old.key,
                    old.identifier.as_str(),
                    old.provider_instance.as_deref(),
                )
            }),
        )
    }))
    .map_err(StateDocumentDecodeError::Validation)?;
    Ok(state)
}
