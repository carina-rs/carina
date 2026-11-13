//! Offline identity observations for comparing #4400 PR heads.
//!
//! Real mode uses unsalted SHA-256 plus byte length. Low-entropy keys and weak
//! secrets are recoverable by dictionary. Real-directory dumps are sensitive
//! local evidence,
//! written under 0600; never attach them to PRs or CI artifacts. Publish only
//! the comparison verdict. RawSynthetic accepts any directory with supplied
//! factories; use it only for generated fixtures, never private configurations.
//! Dump nodes accept only Disclosed values, integers, and their containers.
//! Plaintext comes only from static literals, in-root source paths, or registered
//! schema/type/attribute names. Unchecked authored names are fingerprinted.
//! Provider and resource type names are the schema's names.
//! Block-alias attribute names are shown canonically.
//! Prefix entries are ordered by authored name.
//! Outside-root paths disclose only the fixed marker and a path fingerprint.
//! Exact consecutive computations per owner (including SimHash features)
//! collapse; distinct computations retain their order without pinning pass counts.
//! The production directory/module pipeline uses skip-resource-validation mode
//! (`validate_config_pair(.., true, ..)`); this is not a full validation report.
//! Secret material comes from `SecretPayload::hash(None)`. The production
//! persistence routes `merge_secrets_into_provider_json_inner -> inner.hash`
//! and `persist_secret_json -> hash_secret_value -> payload.hash` use the same
//! PayloadHash hook. Their `state_value_snapshot(..).to_value_at("secret")`
//! preparation before hashing is not observed by this probe.
//!
//! A deferred `for` contributes nothing from its body: no resource entries,
//! anonymous or loop-element identities, keyed addresses, module prefixes, or
//! secret material. Secrets not known offline have `status: unavailable`; a
//! known secret whose production hash fails has `status: hash-error`.
//! Deferred directories are covered by plan parity (`scripts/validate-parity.py`),
//! not by this probe. Partial dumps identify exactly which iterable sites defer.
//! Application diagnostics are reported by error kind only. Run `carina validate`
//! on the directory for details; those diagnostics may quote authored values.
//! The paired validation pipeline has no warning-sink parameter: its own stderr
//! warnings are outside the probe's control. Never publish captured stderr;
//! the ignored real-directory test captures it without echoing its contents.
//! Main and candidate must probe the SAME configuration checkout path: preparatory
//! loop observations include absolute-root bytes. Those production writes are
//! recorded unchanged, not normalised or re-encoded.
//! Calls through verify_hash or value_walk::hash_snapshot outside an identity
//! owner record shared-encoder material as Unattributed and fail the probe closed.
//! Those producer paths are not deliberately driven here.
//! No provider runtime, backend, state read, plan, or download is involved.

use super::{LoadedProviderFactory, ProviderLoad, ProviderNoticeSink, WiringContext};
use crate::error::AppError;
use carina_core::identity_recording::{
    self, HashInput, InputContext, InputOwner, ModuleKey, ResourceKey,
};
use carina_core::resource::{
    ConcreteValue, DeferredValue, ForLoopElement, LoopElementOrigin, LoopKind, LoopOriginKind,
    LoopProvenance, ResourceIdentity, Value,
};
#[cfg(test)]
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

mod disclosure {
    //! The only constructors for dump strings and containers. No arbitrary JSON enters.

    use super::{OfflineProviders, RedactionMode};
    use carina_core::identity_recording::ResourceKey;
    use carina_core::parser::{ModuleCallId, SourceFileId};
    use carina_core::resource::LoopSource;
    use carina_core::schema::{EffectiveField, OwnerBoundResourceSchema, ValidatedResourceSchema};
    use serde::Serialize;
    use serde_json::Value as Json;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    #[serde(transparent)]
    pub struct Disclosed(Json);

    pub(super) fn object(fields: impl IntoIterator<Item = (&'static str, Disclosed)>) -> Disclosed {
        Disclosed(Json::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.0))
                .collect(),
        ))
    }
    pub(super) fn array(values: impl IntoIterator<Item = Disclosed>) -> Disclosed {
        Disclosed(Json::Array(
            values.into_iter().map(|value| value.0).collect(),
        ))
    }
    impl Disclosed {
        pub(super) fn insert(&mut self, key: &'static str, value: Disclosed) {
            self.0
                .as_object_mut()
                .expect("dump object")
                .insert(key.to_owned(), value.0);
        }
    }
    impl From<usize> for Disclosed {
        fn from(value: usize) -> Self {
            Self(Json::from(value))
        }
    }
    impl From<Option<Disclosed>> for Disclosed {
        fn from(value: Option<Disclosed>) -> Self {
            value.unwrap_or(Self(Json::Null))
        }
    }
    impl std::fmt::Display for Disclosed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match &self.0 {
                Json::String(text) => f.write_str(text),
                value => value.fmt(f),
            }
        }
    }

    #[derive(Clone, Copy)]
    pub(super) struct Raw(());
    impl Raw {
        pub(super) fn text(self, value: &str) -> Disclosed {
            Disclosed(Json::String(value.to_owned()))
        }
        pub(super) fn concat(self, parts: impl IntoIterator<Item = Disclosed>) -> Disclosed {
            self.text(
                &parts
                    .into_iter()
                    .map(|part| part.to_string())
                    .collect::<String>(),
            )
        }
    }
    #[derive(Clone, Copy)]
    pub(super) enum Disclosure {
        Raw(Raw),
        Sha256,
    }

    // These tokens cannot be created by renderers from an arbitrary &str.
    struct SchemaName<'a>(&'a str);
    #[derive(Clone, Debug)]
    pub(super) struct RelativeSourcePath(String);
    pub(super) struct SourcePaths {
        root: carina_core::paths::PersistedPath,
    }
    impl SourcePaths {
        pub(super) fn new(directory: &Path) -> std::io::Result<Self> {
            Ok(Self {
                root: carina_core::paths::PersistedPath::new(directory)?,
            })
        }
        pub(super) fn relative(&self, file: &SourceFileId) -> RelativeSourcePath {
            self.relative_path(file.path())
        }
        pub(super) fn module(&self, call: &ModuleCallId) -> RelativeSourcePath {
            self.relative(call.source_file_id())
        }
        pub(super) fn resource(&self, key: &ResourceKey) -> RelativeSourcePath {
            self.relative_path(key.source().0)
        }
        fn relative_path(&self, path: &Path) -> RelativeSourcePath {
            RelativeSourcePath(
                carina_core::paths::PersistedPath::new(path)
                    .expect("parser source locator")
                    .lexical_relative_to(&self.root)
                    .expect("source and root share an anchor")
                    .to_string(),
            )
        }
        pub(super) fn source_file(&self, source: &LoopSource) -> RelativeSourcePath {
            let path = Path::new(source.file());
            let path: PathBuf = if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.root.to_path_buf().join(path)
            };
            self.relative_path(&path)
        }
    }

    enum DisclosureKind<'a> {
        FixedLiteral(&'static str),
        RelativePath(&'a RelativeSourcePath),
        SchemaName(SchemaName<'a>),
        Fingerprint(&'a str),
    }
    impl Disclosure {
        pub(super) fn from_mode(mode: RedactionMode) -> (OfflineProviders, Self) {
            match mode {
                RedactionMode::RawSynthetic(factories) => {
                    (OfflineProviders::Factories(factories), Self::Raw(Raw(())))
                }
                RedactionMode::Sha256(providers) => (providers, Self::Sha256),
            }
        }
        fn disclose(self, kind: DisclosureKind<'_>) -> Disclosed {
            match kind {
                DisclosureKind::FixedLiteral(value) => Disclosed(Json::String(value.to_owned())),
                DisclosureKind::SchemaName(SchemaName(value)) => {
                    Disclosed(Json::String(value.to_owned()))
                }
                DisclosureKind::RelativePath(path) => match self {
                    Self::Raw(raw) => raw.text(&path.0),
                    Self::Sha256
                        if Path::new(&path.0).components().next()
                            == Some(std::path::Component::ParentDir) =>
                    {
                        object([
                            ("kind", self.fixed("outside-root")),
                            ("path", fingerprint(path.0.as_bytes())),
                        ])
                    }
                    Self::Sha256 => Disclosed(Json::String(path.0.clone())),
                },
                DisclosureKind::Fingerprint(value) => match self {
                    Self::Raw(raw) => raw.text(value),
                    Self::Sha256 => fingerprint(value.as_bytes()),
                },
            }
        }
        pub(super) fn fixed(self, value: &'static str) -> Disclosed {
            self.disclose(DisclosureKind::FixedLiteral(value))
        }
        pub(super) fn path(self, value: &RelativeSourcePath) -> Disclosed {
            self.disclose(DisclosureKind::RelativePath(value))
        }
        pub(super) fn opaque(self, value: &str) -> Disclosed {
            self.disclose(DisclosureKind::Fingerprint(value))
        }
        pub(super) fn schema_names(
            self,
            schema: OwnerBoundResourceSchema<'_>,
        ) -> (Disclosed, Disclosed) {
            (
                self.disclose(DisclosureKind::SchemaName(SchemaName(schema.owner()))),
                self.disclose(DisclosureKind::SchemaName(SchemaName(
                    schema.schema().as_schema().resource_type(),
                ))),
            )
        }
        pub(super) fn attribute(
            self,
            schema: &ValidatedResourceSchema,
            authored: &str,
        ) -> Disclosed {
            match schema.effective_attribute_by_authored_name(authored) {
                Some(attribute) => {
                    self.disclose(DisclosureKind::SchemaName(SchemaName(attribute.name())))
                }
                None => self.opaque(authored),
            }
        }
        pub(super) fn field(self, field: &EffectiveField<'_>) -> Disclosed {
            self.disclose(DisclosureKind::SchemaName(SchemaName(field.name())))
        }
    }
    fn fingerprint(value: &[u8]) -> Disclosed {
        let carina_core::identity_recording::Fingerprint { sha256, length } =
            carina_core::identity_recording::fingerprint(value);
        object([
            ("sha256", Disclosed(Json::String(sha256))),
            ("length", length.into()),
        ])
    }
    pub(super) fn bytes(value: &[u8], disclosure: Disclosure) -> Disclosed {
        match disclosure {
            Disclosure::Raw(raw) => object([("hex", raw.text(&hex(value)))]),
            Disclosure::Sha256 => fingerprint(value),
        }
    }
    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(hex, "{byte:02x}").expect("String");
        }
        hex
    }
}

pub use disclosure::Disclosed;
use disclosure::{Disclosure, Raw, SourcePaths, array, bytes, object};

#[cfg(test)]
mod tests;

pub enum OfflineProviders {
    Factories(Vec<Box<dyn carina_core::provider::ProviderFactory>>),
    /// Flat WASMs, one installed project/`.carina/providers`, or an infra
    /// checkout containing a separate installed set at each configuration.
    ArtifactDirectory(PathBuf),
}

/// No default. RawSynthetic accepts ANY directory with caller-supplied
/// factories. Real-directory batch runs always select fingerprints.
pub enum RedactionMode {
    RawSynthetic(Vec<Box<dyn carina_core::provider::ProviderFactory>>),
    Sha256(OfflineProviders),
}

#[derive(thiserror::Error)]
pub enum ProbeError {
    #[error("Application")]
    Application(#[source] Box<AppError>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("incomplete offline identity probe: {0}")]
    Incomplete(IncompleteProbe),
    #[error("skipped: directory contains no .crn files")]
    NoConfiguration,
    #[error(transparent)]
    DirectoryFailures(#[from] DirectoryFailures),
}
impl std::fmt::Debug for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl From<AppError> for ProbeError {
    fn from(error: AppError) -> Self {
        Self::Application(Box::new(error))
    }
}

/// Missing observations are data, never a serialized dump in an error message.
#[derive(Debug)]
pub struct IncompleteProbe {
    pub missing: Vec<String>,
    pub dump_path: Option<PathBuf>,
}
impl std::fmt::Display for IncompleteProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.missing.join("; "))?;
        if let Some(path) = &self.dump_path {
            write!(f, "; partial dump: {}", path.display())?;
        }
        Ok(())
    }
}
impl ProbeError {
    fn incomplete(message: impl Into<String>) -> Self {
        Self::Incomplete(IncompleteProbe {
            missing: vec![message.into()],
            dump_path: None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredSite {
    pub file: Disclosed,
    pub span: std::ops::Range<usize>,
}

#[derive(Debug)]
pub enum DirectoryStatus {
    Ok,
    Skipped,
    Partial { deferred: Vec<DeferredSite> },
    Failed(ProbeError),
}
impl std::fmt::Display for DirectoryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Skipped => f.write_str("skipped (no .crn files)"),
            Self::Partial { deferred } => write!(f, "partial ({} deferred sites)", deferred.len()),
            Self::Failed(error) => write!(f, "failed ({error})"),
        }
    }
}
#[derive(Debug)]
pub struct DirectoryResult {
    pub directory: PathBuf,
    pub status: DirectoryStatus,
}
#[derive(Debug, Default)]
pub struct DirectoryReport(Vec<DirectoryResult>);
impl DirectoryReport {
    pub fn results(&self) -> &[DirectoryResult] {
        &self.0
    }
}
#[derive(Debug)]
pub struct DirectoryFailures(DirectoryReport);
impl DirectoryFailures {
    pub fn report(&self) -> &DirectoryReport {
        &self.0
    }
    pub fn failures(&self) -> impl Iterator<Item = &DirectoryResult> {
        self.0
            .0
            .iter()
            .filter(|result| matches!(result.status, DirectoryStatus::Failed(_)))
    }
}
impl std::fmt::Display for DirectoryFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} offline identity probe directories failed",
            self.failures().count()
        )?;
        for result in self.failures() {
            if let DirectoryStatus::Failed(error) = &result.status {
                write!(f, "\n{}: {error}", result.directory.display())?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for DirectoryFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failures().find_map(|result| match &result.status {
            DirectoryStatus::Failed(error) => Some(error as &(dyn std::error::Error + 'static)),
            DirectoryStatus::Ok | DirectoryStatus::Skipped | DirectoryStatus::Partial { .. } => {
                None
            }
        })
    }
}

#[derive(Debug)]
pub struct ProbeOutcome {
    pub dump: String,
    pub deferred: Vec<DeferredSite>,
}

struct ProbeDocument {
    dump: Disclosed,
    missing: Vec<String>,
    deferred: Vec<DeferredSite>,
}
impl ProbeDocument {
    fn complete(self, dump_path: Option<PathBuf>) -> Result<ProbeOutcome, ProbeError> {
        if self.missing.is_empty() {
            Ok(ProbeOutcome {
                dump: format!("{}\n", serde_json::to_string_pretty(&self.dump)?),
                deferred: self.deferred,
            })
        } else {
            Err(ProbeError::Incomplete(IncompleteProbe {
                missing: self.missing,
                dump_path,
            }))
        }
    }
}

struct PreparedResource {
    dump: Disclosed,
    address: String,
    key: ResourceKey,
    loop_identities: Vec<ResourceIdentity>,
    requires_identity: bool,
    required_modules: Vec<ModuleKey>,
}
#[derive(Default)]
struct PreparedProbe {
    resources: Vec<PreparedResource>,
    compositions: Vec<ModuleKey>,
    missing: Vec<String>,
    deferred: Vec<DeferredSite>,
}

/// Run synchronously outside an async runtime. Provider schemas and environment
/// are explicit inputs. Byte disclosure is a required, closed API choice.
pub fn probe_directory(
    directory: &Path,
    environment: &carina_core::EnvironmentSnapshot,
    mode: RedactionMode,
) -> Result<ProbeOutcome, ProbeError> {
    capture_directory(directory, environment, mode)?.complete(None)
}

fn capture_directory(
    directory: &Path,
    environment: &carina_core::EnvironmentSnapshot,
    mode: RedactionMode,
) -> Result<ProbeDocument, ProbeError> {
    let (providers, disclosure) = Disclosure::from_mode(mode);
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    executor.block_on(async {
        // One pinned source spelling is used for loading and every rendering.
        let directory = carina_core::paths::ResolvedPath::new(directory)?;
        let paths = SourcePaths::new(&directory.source_label())?;
        let (prepared, inputs) = identity_recording::record(|| {
            prepare(&directory, providers, environment, &paths, disclosure)
        });
        match prepared {
            Ok(prepared) => finish_dump(prepared, inputs, &paths, disclosure),
            Err(ProbeError::Incomplete(details)) => finish_dump(
                PreparedProbe {
                    missing: details.missing,
                    ..PreparedProbe::default()
                },
                inputs,
                &paths,
                disclosure,
            ),
            Err(error) => Err(error),
        }
    })
}

fn prepare(
    directory: &carina_core::paths::ResolvedPath,
    providers: OfflineProviders,
    environment: &carina_core::EnvironmentSnapshot,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<PreparedProbe, ProbeError> {
    if directory.crn_files()?.is_empty() {
        return Err(ProbeError::NoConfiguration);
    }
    let runtime =
        crate::test_support::TestCommandRuntime::new().with_environment(environment.clone());
    let loaded = carina_core::config_loader::load_configuration_with_config(
        directory,
        &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
        &carina_core::schema::SchemaRegistry::new(),
    )
    .map_err(AppError::from)?;
    let factories = match providers {
        OfflineProviders::Factories(factories) => LoadedProviderFactory::for_test(factories),
        OfflineProviders::ArtifactDirectory(artifacts) => {
            load_artifacts(&runtime, directory, &loaded.parsed.providers, &artifacts)?
        }
    };
    let context = WiringContext::new(&runtime, factories, &loaded.parsed.providers)?;
    // Fail before identity preparation can silently skip an unregistered type.
    for resource in &loaded.parsed.resources {
        if context.schemas().get_for(resource).is_none() {
            return Err(ProbeError::incomplete(format!(
                "missing schema at {}",
                paths.declaration(identity_recording::resource_location(resource), disclosure)?
            )));
        }
    }
    let validated = ProviderLoad {
        context,
        load_errors: HashMap::new(),
    }
    .validate_config_pair(
        loaded.parsed,
        loaded.unresolved_parsed,
        directory,
        true,
        &loaded.duplicate_declarations,
    )?;
    let parsed = validated.resources().parsed();
    let deferred = parsed
        .deferred_for_expressions()
        .iter()
        .map(|expression| {
            let location = expression
                .location()
                .recording_iterable_location()
                .ok_or_else(|| ProbeError::incomplete("deferred site has no parser span"))?;
            Ok(DeferredSite {
                file: disclosure.path(&paths.relative(location.file())),
                span: location.span().start_byte()..location.span().end_byte(),
            })
        })
        .collect::<Result<Vec<_>, ProbeError>>()?;
    let mut missing = Vec::new();
    if parsed.resources.is_empty() && deferred.is_empty() {
        missing.push("zero resources enumerated".to_owned());
    }
    let mut resources = Vec::new();
    for (ordinal, resource) in parsed.resources.iter().enumerate() {
        let prepared = (|| {
            let location = identity_recording::resource_location(resource)
                .ok_or_else(|| ProbeError::incomplete("resource has no declaration location"))?;
            let declaration = paths.declaration(Some(location), disclosure)?;
            let bound_schema =
                validated
                    .context()
                    .schemas()
                    .get_for(resource)
                    .ok_or_else(|| {
                        ProbeError::incomplete(format!("missing schema at {declaration}"))
                    })?;
            let schema = bound_schema.schema();
            if resource.id.identity().is_none() {
                return Err(ProbeError::incomplete(format!(
                    "unassigned resource identity at {declaration}"
                )));
            }
            let key = identity_recording::resource_key(resource).ok_or_else(|| {
                ProbeError::incomplete(format!(
                    "resource has no parser-assigned identity at {declaration}"
                ))
            })?;
            let span = location.span();
            let mut instances = Vec::new();
            let mut required_modules = Vec::new();
            if let Some(module) = resource
                .module_source
                .as_ref()
                .and_then(|source| source.module_call())
            {
                let keys =
                    ModuleKey::prefixes(module.module_call_path().calls().map(|call| &call.id));
                for (call, key) in module.module_call_path().calls().zip(keys) {
                    if call.binding.is_none() {
                        required_modules.push(key);
                    }
                    let location = call.id.declaration_location();
                    instances.push(object([
                        ("instance", disclosure.opaque(&call.instance)),
                        (
                            "declaration",
                            disclosure.path(&paths.relative(location.file())),
                        ),
                        ("start_byte", location.span().start_byte().into()),
                    ]));
                }
            }
            let basis = if resource.binding.is_some() {
                "named"
            } else {
                match carina_core::identifier::classify_anonymous_identity_basis(
                    resource,
                    validated.context().schemas(),
                ) {
                    Some(carina_core::identifier::AnonymousIdentityBasis::Stable) => "stable",
                    Some(carina_core::identifier::AnonymousIdentityBasis::AttributeDerived) => {
                        "attribute-derived"
                    }
                    Some(
                        carina_core::identifier::AnonymousIdentityBasis::IncompleteDeclaredIdentity,
                    ) => {
                        return Err(ProbeError::incomplete("incomplete declared identity"));
                    }
                    None => {
                        return Err(ProbeError::incomplete("identity basis has no schema"));
                    }
                }
            };
            let mut secrets = Vec::new();
            for (attribute, value) in &resource.attributes {
                let effective = schema
                    .effective_attribute_by_authored_name(attribute)
                    .map(|attribute| attribute.value_type());
                observe_secrets(
                    value,
                    &SecretPath(vec![SecretPathPart::Attribute(
                        disclosure.attribute(schema, attribute),
                    )]),
                    effective,
                    &mut secrets,
                    paths,
                    disclosure,
                )?;
            }
            let mut prefixes: Vec<_> = resource.prefixes.iter().collect();
            prefixes.sort_by_key(|(name, _)| *name);
            let prefixes = array(prefixes.into_iter().map(|(name, value)| {
                object([
                    ("attribute", disclosure.attribute(schema, name)),
                    ("value", disclosure.opaque(value)),
                ])
            }));
            let provenance = paths.loop_provenance(&resource.loop_provenance, disclosure);
            let (provider, resource_type) = disclosure.schema_names(bound_schema);
            let mut resource_id = object([
                ("provider", provider),
                ("resource_type", resource_type),
                (
                    "identity",
                    disclosure.opaque(resource.id.identity_str().expect("resolved identity")),
                ),
            ]);
            if let Some(binding) = &resource.id.provider_instance {
                resource_id.insert("provider_instance", disclosure.opaque(binding));
            }
            Ok::<_, ProbeError>(PreparedResource {
                address: declaration,
                key,
                loop_identities: identity_recording::loop_system_identities(
                    &resource.loop_provenance,
                ),
                requires_identity: resource.binding.is_none(),
                required_modules,
                dump: object([
                    ("ordinal", ordinal.into()),
                    (
                        "declaration",
                        object([
                            ("file", disclosure.path(&paths.relative(location.file()))),
                            ("start_byte", span.start_byte().into()),
                            ("end_byte", span.end_byte().into()),
                            ("line", span.start_line().into()),
                            ("column", span.start_column().into()),
                        ]),
                    ),
                    (
                        "expansion",
                        object([("instances", array(instances)), ("loop", provenance)]),
                    ),
                    ("identity_basis", disclosure.fixed(basis)),
                    ("resource_id", resource_id),
                    ("address", disclosure.opaque(&resource.id.to_string())),
                    ("resolved_prefixes", prefixes),
                    ("secret_material", array(secrets)),
                ]),
            })
        })();
        match prepared {
            Ok(resource) => resources.push(resource),
            Err(ProbeError::Incomplete(details)) => missing.extend(details.missing),
            Err(error) => return Err(error),
        }
    }
    let compositions = parsed
        .compositions
        .iter()
        .flat_map(|composition| {
            ModuleKey::prefixes(composition.call_path().calls().map(|call| &call.id))
        })
        .collect();
    Ok(PreparedProbe {
        resources,
        compositions,
        missing,
        deferred,
    })
}

fn finish_dump(
    prepared: PreparedProbe,
    inputs: Vec<HashInput>,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<ProbeDocument, ProbeError> {
    let PreparedProbe {
        mut resources,
        compositions,
        missing: mut failures,
        deferred,
    } = prepared;
    let mut claimed = vec![false; inputs.len()];
    let mut keys = HashSet::new();
    for resource in &mut resources {
        let address = &resource.address;
        if !keys.insert(resource.key.clone()) {
            failures.push(format!("duplicate system resource identity for {address}"));
        }
        for module in &resource.required_modules {
            if !inputs.iter().any(|input| {
                matches!(&input.owner, InputOwner::Module(recorded) if recorded == module)
                    && input.boundary == "simhash"
                    && !input.finishes.is_empty()
            }) {
                failures.push(format!("missing module prefix hash inputs for {address}"));
            }
        }
        let mut loops: HashSet<_> = resource.loop_identities.iter().cloned().collect();
        loop {
            let previous = loops.len();
            for input in &inputs {
                if let InputOwner::Loop {
                    identity,
                    parent: Some(parent),
                } = &input.owner
                    && loops.contains(identity)
                {
                    loops.insert(parent.clone());
                }
            }
            if loops.len() == previous {
                break;
            }
        }
        let mut required_loops: Vec<_> = loops.iter().collect();
        required_loops.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for identity in required_loops {
            if !inputs.iter().any(|input| matches!(&input.owner, InputOwner::Loop { identity: recorded, .. } if recorded == identity)) {
                failures.push(format!("missing loop identity hash inputs for {address}"));
            }
        }
        let mut rendered = Vec::new();
        let mut primary = false;
        for (index, input) in inputs.iter().enumerate() {
            let owns = match &input.owner {
                InputOwner::Resource(key) => key == &resource.key,
                InputOwner::Module(module) => module.owns(&resource.key),
                InputOwner::Loop { identity, .. } => loops.contains(identity),
                InputOwner::PayloadHash | InputOwner::Unattributed => false,
            };
            if !owns {
                continue;
            }
            claimed[index] = true;
            if matches!(&input.owner, InputOwner::Resource(key) if key == &resource.key)
                && matches!(
                    input.boundary,
                    "declared-identity" | "create-only" | "simhash"
                )
                && !input.finishes.is_empty()
            {
                primary = true;
            }
            rendered.push(input);
        }
        if resource.requires_identity && !primary {
            failures.push(format!("missing identity hash inputs for {address}"));
        }
        resource.dump.insert(
            "hash_inputs",
            array(
                collapse_observations(rendered)
                    .into_iter()
                    .map(|input| render_input(input, paths, disclosure)),
            ),
        );
    }
    // A SimHash result alone cannot prove that feature hashing was observed:
    // later parallelization must not make off-thread writes disappear.
    for (index, input) in inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| input.boundary == "simhash")
    {
        let features = inputs[index + 1..]
            .iter()
            .take_while(|next| next.boundary != "simhash")
            .filter(|next| {
                next.owner == input.owner
                    && next.boundary == "simhash-feature"
                    && !next.finishes.is_empty()
            })
            .count();
        if input.feature_count != Some(features) {
            failures.push(format!(
                "incomplete SimHash feature recording: expected {:?}, observed {features}",
                input.feature_count
            ));
        }
    }
    let mut unattributed = Vec::new();
    for input in collapse_observations(
        inputs
            .iter()
            .zip(claimed)
            .filter_map(|(input, claimed)| (!claimed).then_some(input)),
    ) {
        // Preparatory loop identities are produced while parsing/rebasing
        // templates. They need not survive the expanded graph. Keep every
        // observation explicitly, with this reason; every retained loop and
        // parent was independently required above, so missing final inputs fail.
        let reason = match &input.owner {
            InputOwner::Loop { .. } => {
                Some("preparatory loop template identity not retained by the expanded graph")
            }
            InputOwner::Module(key) if compositions.contains(key) => {
                Some("composition call has no managed resource descendants")
            }
            InputOwner::PayloadHash => {
                Some("production secret payload hash outside a resource identity pass")
            }
            InputOwner::Resource(_) | InputOwner::Module(_) | InputOwner::Unattributed => None,
        };
        if reason.is_none() {
            failures.push("unattributed hash inputs".to_owned());
        }
        unattributed.push(object([
            (
                "reason",
                reason.map(|reason| disclosure.fixed(reason)).into(),
            ),
            ("input", render_input(input, paths, disclosure)),
        ]));
    }
    let dump = object([
        ("format", disclosure.fixed("carina-identity-probe-v3")),
        (
            "redaction",
            disclosure.fixed(match disclosure {
                Disclosure::Raw(_) => "raw-synthetic",
                Disclosure::Sha256 => "sha256",
            }),
        ),
        (
            "resources",
            array(resources.into_iter().map(|resource| resource.dump)),
        ),
        ("unattributed", array(unattributed)),
        (
            "status",
            object([
                (
                    "kind",
                    disclosure.fixed(if !failures.is_empty() {
                        "failed"
                    } else if deferred.is_empty() {
                        "ok"
                    } else {
                        "partial"
                    }),
                ),
                (
                    "deferred",
                    array(deferred.iter().map(|site| {
                        object([
                            ("file", site.file.clone()),
                            (
                                "span",
                                object([
                                    ("start", site.span.start.into()),
                                    ("end", site.span.end.into()),
                                ]),
                            ),
                        ])
                    })),
                ),
            ]),
        ),
    ]);
    let mut seen = HashSet::new();
    failures.retain(|message| seen.insert(message.clone()));
    Ok(ProbeDocument {
        dump,
        missing: failures,
        deferred,
    })
}

fn collapse_observations<'a>(
    observations: impl IntoIterator<Item = &'a HashInput>,
) -> Vec<&'a HashInput> {
    let items: Vec<_> = observations.into_iter().collect();
    let mut previous = HashMap::new();
    let mut result = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let input = items[start];
        let mut end = start + 1;
        if input.boundary == "simhash" {
            while end < items.len()
                && items[end].owner == input.owner
                && items[end].boundary == "simhash-feature"
            {
                end += 1;
            }
        }
        let computation = &items[start..end];
        if previous.insert(&input.owner, computation) != Some(computation) {
            result.extend_from_slice(computation);
        }
        start = end;
    }
    result
}

#[derive(Clone)]
enum SecretPathPart {
    Attribute(Disclosed),
    MapKey(Disclosed),
    Index(usize),
}
#[derive(Clone)]
struct SecretPath(Vec<SecretPathPart>);
impl SecretPath {
    fn child(&self, part: SecretPathPart) -> Self {
        let mut child = self.clone();
        child.0.push(part);
        child
    }
    fn render(&self, disclosure: Disclosure) -> Disclosed {
        match disclosure {
            Disclosure::Raw(raw) => {
                let mut parts = Vec::new();
                for part in &self.0 {
                    match part {
                        SecretPathPart::Attribute(name) | SecretPathPart::MapKey(name) => {
                            if !parts.is_empty() {
                                parts.push(disclosure.fixed("."));
                            }
                            parts.push(name.clone());
                        }
                        SecretPathPart::Index(index) => parts.extend([
                            disclosure.fixed("["),
                            (*index).into(),
                            disclosure.fixed("]"),
                        ]),
                    }
                }
                raw.concat(parts)
            }
            Disclosure::Sha256 => array(self.0.iter().map(|part| match part {
                SecretPathPart::Attribute(name) => object([("attribute", name.clone())]),
                SecretPathPart::MapKey(key) => object([("map_key", key.clone())]),
                SecretPathPart::Index(index) => object([("index", (*index).into())]),
            })),
        }
    }
}

fn observe_secrets(
    value: &Value,
    path: &SecretPath,
    effective: Option<carina_core::schema::EffectiveType<'_>>,
    out: &mut Vec<Disclosed>,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<(), ProbeError> {
    match value {
        Value::Deferred(DeferredValue::Secret(payload)) => {
            let known = value
                .knowledge(&carina_core::resource::knowledge::KnowledgeContext::validation())
                .ok()
                .and_then(|proof| proof.into_classification().into_known().ok())
                .is_some();
            let mut fields = vec![
                ("path", path.render(disclosure)),
                ("producer", disclosure.fixed("secret-payload-hash")),
            ];
            if !known {
                fields.extend([
                    ("status", disclosure.fixed("unavailable")),
                    ("reason", disclosure.fixed("secret is not known offline")),
                ]);
            } else {
                let (result, inputs) = identity_recording::record(|| payload.hash(None));
                match result {
                    Ok(_) => {
                        if inputs.is_empty() {
                            return Err(ProbeError::incomplete(
                                "missing secret material recording",
                            ));
                        }
                        fields.extend([
                            ("status", disclosure.fixed("available")),
                            (
                                "inputs",
                                array(
                                    collapse_observations(&inputs)
                                        .into_iter()
                                        .map(|input| render_input(input, paths, disclosure)),
                                ),
                            ),
                        ]);
                    }
                    Err(_) => fields.extend([
                        ("status", disclosure.fixed("hash-error")),
                        (
                            "reason",
                            disclosure.fixed("production hashing failed for a known secret"),
                        ),
                    ]),
                }
            }
            out.push(object(fields));
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for (index, item) in items.iter().enumerate() {
                observe_secrets(
                    item,
                    &path.child(SecretPathPart::Index(index)),
                    effective.as_ref().and_then(|ty| ty.list_element()),
                    out,
                    paths,
                    disclosure,
                )?;
            }
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            for (key, item) in entries {
                let field = effective
                    .as_ref()
                    .filter(|ty| {
                        matches!(ty.shape(), carina_core::schema::Shape::Struct { name: _ })
                    })
                    .and_then(|ty| {
                        ty.field_candidates_with_budget(
                            &mut carina_core::schema::ShapeWalkBudget::new(64),
                        )
                        .into_iter()
                        .find(|field| field.name() == key)
                    });
                let (part, child_type) = match field {
                    Some(field) => (
                        SecretPathPart::Attribute(disclosure.field(&field)),
                        Some(field.value_type()),
                    ),
                    None => (
                        SecretPathPart::MapKey(disclosure.opaque(key)),
                        effective.as_ref().and_then(|ty| ty.map_value()),
                    ),
                };
                observe_secrets(item, &path.child(part), child_type, out, paths, disclosure)?;
            }
        }
        _ if carina_core::value::contains_secret(value) => out.push(object([
            ("path", path.render(disclosure)),
            ("status", disclosure.fixed("unavailable")),
            (
                "reason",
                disclosure.fixed("secret remains inside a deferred expression"),
            ),
        ])),
        _ => {}
    }
    Ok(())
}

fn load_artifacts(
    runtime: &crate::runtime::CommandRuntime,
    directory: &carina_core::paths::ResolvedPath,
    providers: &[carina_core::parser::ProviderConfig<
        carina_core::resource::ParserPlaceholder,
        carina_core::resource::RelativeBindingName,
    >],
    artifacts: &Path,
) -> Result<Vec<LoadedProviderFactory>, ProbeError> {
    if !artifacts.is_dir() {
        return Err(ProbeError::incomplete(format!(
            "missing provider artifact directory: {}",
            artifacts.display()
        )));
    }
    let artifacts = carina_core::paths::ResolvedPath::new(artifacts)?;
    let mut overrides = HashMap::new();
    for provider in providers.iter().filter(|provider| provider.is_default()) {
        let candidates: Vec<_> = [
            format!("{}.wasm", provider.name),
            format!("carina_provider_{}.wasm", provider.name),
            format!("carina-provider-{}.wasm", provider.name),
        ]
        .into_iter()
        .map(|name| artifacts.join(name))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
        match candidates.as_slice() {
            [path] => {
                overrides.insert(provider.name.clone(), path.persisted_path().to_path_buf());
            }
            [] => {}
            _ => {
                return Err(ProbeError::incomplete(format!(
                    "multiple artifacts for provider {}",
                    provider.name
                )));
            }
        }
    }
    let effective = super::schema_check_provider_configs(providers, &overrides)?;
    let base = if artifacts
        .display()
        .file_name()
        .is_some_and(|name| name == "providers")
        && artifacts.parent().is_some_and(|path| {
            path.display()
                .file_name()
                .is_some_and(|name| name == ".carina")
        }) {
        artifacts
            .parent()
            .and_then(|parent| parent.parent())
            .expect("installed provider root")
    } else {
        artifacts
    };
    // Relative file sources still belong to the configuration directory.
    let mut effective = effective;
    for provider in &mut effective {
        if !overrides.contains_key(&provider.name)
            && let Some(carina_core::provider_source::ProviderSource::File(source)) =
                &provider.source
        {
            let path = directory
                .join(source.path())?
                .persisted_path()
                .to_path_buf();
            let mut one = HashMap::new();
            one.insert(provider.name.clone(), path);
            *provider = super::schema_check_provider_configs(std::slice::from_ref(provider), &one)?
                .remove(0);
        }
    }
    let (factories, errors) = super::build_schema_only_factories_from_providers(
        runtime,
        &effective,
        &base,
        &mut ProviderNoticeSink::new(|_| {}),
    )?;
    if !errors.is_empty() {
        return Err(
            crate::commands::collapse_errors(super::sorted_provider_load_errors(&errors)).into(),
        );
    }
    if factories.is_empty() {
        return Err(ProbeError::incomplete("no offline provider schemas loaded"));
    }
    Ok(factories)
}

impl SourcePaths {
    fn declaration(
        &self,
        location: Option<&carina_core::parser::DeclarationLocation>,
        disclosure: Disclosure,
    ) -> Result<String, ProbeError> {
        let location = location
            .ok_or_else(|| ProbeError::incomplete("resource has no declaration location"))?;
        Ok(format!(
            "{}:{}",
            disclosure.path(&self.relative(location.file())),
            location.span().start_byte()
        ))
    }
    fn module_context(
        &self,
        call: &carina_core::parser::ModuleCallId,
        disclosure: Disclosure,
    ) -> Disclosed {
        match disclosure {
            Disclosure::Raw(raw) => self.raw_module_context(call, raw),
            Disclosure::Sha256 => object([
                ("kind", disclosure.fixed("module")),
                ("file", disclosure.path(&self.module(call))),
                ("start_byte", call.call_span().start_byte().into()),
                (
                    "expansion",
                    match call.expansion() {
                        carina_core::parser::CallExpansion::Direct => None,
                        carina_core::parser::CallExpansion::Expanded(expansion) => {
                            Some(disclosure.opaque(expansion.as_str()))
                        }
                    }
                    .into(),
                ),
            ]),
        }
    }
    fn raw_module_context(&self, call: &carina_core::parser::ModuleCallId, raw: Raw) -> Disclosed {
        let mode = Disclosure::Raw(raw);
        let mut parts = vec![
            mode.fixed("module:<root>/"),
            mode.path(&self.module(call)),
            mode.fixed(":"),
            call.call_span().start_byte().into(),
        ];
        if let carina_core::parser::CallExpansion::Expanded(expansion) = call.expansion() {
            parts.extend([mode.fixed(":"), raw.text(expansion.as_str())]);
        }
        raw.concat(parts)
    }
    fn context(&self, context: &InputContext, disclosure: Disclosure) -> Disclosed {
        match context {
            InputContext::Resource(Some(location)) => {
                let file = disclosure.path(&self.relative(location.file()));
                let byte = location.span().start_byte();
                match disclosure {
                    Disclosure::Raw(raw) => raw.concat([
                        disclosure.fixed("resource:<root>/"),
                        file,
                        disclosure.fixed(":"),
                        byte.into(),
                    ]),
                    Disclosure::Sha256 => object([
                        ("kind", disclosure.fixed("resource")),
                        ("file", file),
                        ("start_byte", byte.into()),
                    ]),
                }
            }
            InputContext::Resource(None) => disclosure.fixed("resource:unavailable"),
            InputContext::Module(call) => self.module_context(call, disclosure),
            InputContext::PayloadHash => disclosure.fixed("secret-payload-hash"),
        }
    }
    fn contexts(&self, input: &HashInput, disclosure: Disclosure) -> Disclosed {
        array(match &input.owner {
            InputOwner::Resource(key) => {
                let (_, ordinal) = key.source();
                let file = disclosure.path(&self.resource(key));
                vec![match disclosure {
                    Disclosure::Raw(raw) => raw.concat([
                        disclosure.fixed("resource:<root>/"),
                        file,
                        disclosure.fixed("#"),
                        ordinal.into(),
                    ]),
                    Disclosure::Sha256 => object([
                        ("kind", disclosure.fixed("resource")),
                        ("file", file),
                        ("ordinal", ordinal.into()),
                    ]),
                }]
            }
            InputOwner::Module(key) => key
                .calls()
                .map(|call| self.module_context(call, disclosure))
                .collect(),
            InputOwner::Loop { .. } | InputOwner::PayloadHash | InputOwner::Unattributed => input
                .context
                .iter()
                .map(|context| self.context(context, disclosure))
                .collect(),
        })
    }
    fn loop_provenance(&self, provenance: &LoopProvenance, disclosure: Disclosure) -> Disclosed {
        let (direct, modules) = provenance.recording_parts();
        let mut fields = Vec::new();
        if let Some(origin) = direct {
            fields.push(("direct", self.loop_origin(origin, disclosure)));
        }
        let modules: Vec<_> = modules
            .map(|(origin, prefix)| {
                object([
                    ("origin", self.loop_origin(origin, disclosure)),
                    ("current_prefix", disclosure.opaque(prefix.as_str())),
                ])
            })
            .collect();
        if !modules.is_empty() {
            fields.push(("module_prefixes", array(modules)));
        }
        object(fields)
    }
    fn loop_origin(&self, origin: &LoopElementOrigin, disclosure: Disclosure) -> Disclosed {
        let (source, legacy, parent) = origin.recording_parts();
        let (source_kind, identity) = source.recording_parts();
        let kind = match origin.kind() {
            LoopOriginKind::For { element } => object([
                ("kind", disclosure.fixed("for")),
                (
                    "element",
                    match element {
                        ForLoopElement::Index(index) => object([
                            ("kind", disclosure.fixed("index")),
                            ("value", (*index).into()),
                        ]),
                        ForLoopElement::Key(key) => object([
                            ("kind", disclosure.fixed("key")),
                            ("value", disclosure.opaque(key)),
                        ]),
                    },
                ),
            ]),
            LoopOriginKind::If => object([("kind", disclosure.fixed("if"))]),
        };
        let mut out = object([
            ("kind", kind),
            (
                "source",
                object([
                    ("display_file", disclosure.path(&self.source_file(source))),
                    ("line", source.line().into()),
                    ("column", source.column().into()),
                    (
                        "kind",
                        disclosure.fixed(match source_kind {
                            LoopKind::For => "for",
                            LoopKind::If => "if",
                        }),
                    ),
                    ("system_identity", disclosure.opaque(identity.as_str())),
                ]),
            ),
            ("legacy", disclosure.opaque(legacy)),
        ]);
        if let Some(parent) = parent {
            out.insert("current_parent", disclosure.opaque(parent.as_str()));
        }
        out
    }
}

fn render_input(input: &HashInput, paths: &SourcePaths, disclosure: Disclosure) -> Disclosed {
    object([
        ("boundary", disclosure.fixed(input.boundary)),
        ("context", paths.contexts(input, disclosure)),
        (
            "writes",
            array(input.writes.iter().map(|write| {
                let mut value = bytes(&write.bytes, disclosure);
                value.insert("operation", disclosure.fixed(write.operation));
                value
            })),
        ),
        (
            "finishes",
            array(input.finishes.iter().map(|finish| {
                object([
                    ("operation", disclosure.fixed(finish.operation)),
                    ("after_writes", finish.after_writes.into()),
                    ("output", bytes(&finish.output, disclosure)),
                ])
            })),
        ),
        (
            "feature_count",
            input.feature_count.map(Disclosed::from).into(),
        ),
    ])
}

/// Batch entry point for real directories. It has no raw-byte mode parameter.
/// Every directory is attempted; successes are written even if others fail.
pub fn probe_real_directories(
    root: &Path,
    directories: &[PathBuf],
    artifacts: &Path,
    output: &Path,
    environment: &carina_core::EnvironmentSnapshot,
) -> Result<DirectoryReport, ProbeError> {
    write_directory_dumps(root, directories, output, |directory| {
        // A checkout may keep a separate installed/locked provider set in each
        // configuration. Reuse that exact set, with no aggregate or download.
        let installed = artifacts
            .join(directory.strip_prefix(root).expect("walked root"))
            .join(".carina/providers");
        let artifacts = if installed.is_dir() {
            &installed
        } else {
            artifacts
        };
        capture_directory(
            directory,
            environment,
            RedactionMode::Sha256(OfflineProviders::ArtifactDirectory(artifacts.to_path_buf())),
        )
    })
}

fn write_directory_dumps(
    root: &Path,
    directories: &[PathBuf],
    output: &Path,
    mut probe: impl FnMut(&Path) -> Result<ProbeDocument, ProbeError>,
) -> Result<DirectoryReport, ProbeError> {
    if output.exists() {
        if !output.is_dir() {
            return Err(ProbeError::incomplete(format!(
                "output root is not a directory: {}",
                output.display()
            )));
        }
        if std::fs::read_dir(output)?.next().transpose()?.is_some() {
            return Err(ProbeError::incomplete(format!(
                "output root is not empty: {}",
                output.display()
            )));
        }
    }
    std::fs::create_dir_all(output)?;
    let mut report = DirectoryReport::default();
    for directory in directories {
        let result = (|| {
            if carina_core::paths::ResolvedPath::new(directory)?
                .crn_files()?
                .is_empty()
            {
                return Err(ProbeError::NoConfiguration);
            }
            let relative = directory.strip_prefix(root).map_err(|_| {
                ProbeError::incomplete("probe directory is outside the supplied root")
            })?;
            let document = probe(directory)?;
            let destination = output.join(relative);
            std::fs::create_dir_all(&destination)?;
            let dump_path = destination.join("identity.json");
            write_private_dump(&dump_path, &document.dump)?;
            let ProbeOutcome { deferred, .. } = document.complete(Some(dump_path))?;
            Ok(if deferred.is_empty() {
                DirectoryStatus::Ok
            } else {
                DirectoryStatus::Partial { deferred }
            })
        })();
        let status = match result {
            Ok(status) => status,
            Err(ProbeError::NoConfiguration) => DirectoryStatus::Skipped,
            Err(error) => DirectoryStatus::Failed(error),
        };
        report.0.push(DirectoryResult {
            directory: directory.clone(),
            status,
        });
    }
    if report
        .0
        .iter()
        .any(|result| matches!(result.status, DirectoryStatus::Failed(_)))
    {
        Err(DirectoryFailures(report).into())
    } else {
        Ok(report)
    }
}

fn write_private_dump(path: &Path, dump: &Disclosed) -> Result<(), ProbeError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer_pretty(&mut file, dump)?;
    file.write_all(b"\n")?;
    Ok(())
}
