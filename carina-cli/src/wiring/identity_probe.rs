//! Offline identity observations for comparing #4400 PR heads.
//!
//! Synthetic callers explicitly choose raw bytes or fingerprints. The real
//! directory entry point only writes SHA-256 and byte lengths, never raw bytes.
//! User-derived loop/map keys, address key parts, expansion discriminators and
//! secret-path map keys are fingerprinted too. Source-level resource types,
//! binding names, schema-declared attribute/field names and relative files stay
//! readable. Exact consecutive observations per owner collapse; distinct
//! observations retain their original order, without pinning pass counts.
//! Secret observations drive both `SecretPayload::hash(None)` and the production
//! snapshot producer (`ProtectedSecret::hash_snapshot`, including map_bindings).
//! No provider runtime, backend, state read, plan, or download is involved.

use super::{LoadedProviderFactory, ProviderLoad, ProviderNoticeSink, WiringContext};
use crate::error::AppError;
use carina_core::identity_recording::{
    self, AddressPart, HashInput, InputContext, InputOwner, ModuleKey, ResourceKey,
};
use carina_core::resource::{ConcreteValue, DeferredValue, ResourceIdentity, Value};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

pub enum OfflineProviders {
    Factories(Vec<Box<dyn carina_core::provider::ProviderFactory>>),
    /// Flat WASMs, one installed project/`.carina/providers`, or an infra
    /// checkout containing a separate installed set at each configuration.
    ArtifactDirectory(PathBuf),
}

/// No default. Raw mode accepts only synthetic in-process factories; it cannot
/// contain an artifact directory. Real-directory runs cannot select raw mode.
pub enum RedactionMode {
    RawSynthetic(Vec<Box<dyn carina_core::provider::ProviderFactory>>),
    Sha256(OfflineProviders),
}

#[derive(Clone, Copy)]
enum Disclosure {
    Raw,
    Sha256,
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error(transparent)]
    Application(#[from] Box<AppError>),
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

#[derive(Debug)]
pub enum DirectoryStatus {
    Ok,
    Skipped,
    Failed(ProbeError),
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
            DirectoryStatus::Ok | DirectoryStatus::Skipped => None,
        })
    }
}

struct ProbeDocument {
    dump: String,
    missing: Vec<String>,
}
impl ProbeDocument {
    fn complete(self, dump_path: Option<PathBuf>) -> Result<String, ProbeError> {
        if self.missing.is_empty() {
            Ok(self.dump)
        } else {
            Err(ProbeError::Incomplete(IncompleteProbe {
                missing: self.missing,
                dump_path,
            }))
        }
    }
}

struct PreparedResource {
    dump: Json,
    key: ResourceKey,
    loop_identities: Vec<ResourceIdentity>,
    requires_identity: bool,
    required_modules: Vec<ModuleKey>,
}
struct PreparedProbe {
    resources: Vec<PreparedResource>,
    compositions: Vec<ModuleKey>,
    missing: Vec<String>,
}

/// Run synchronously outside an async runtime. Provider schemas and environment
/// are explicit inputs. Byte disclosure is a required, closed API choice.
pub fn probe_directory(
    directory: &Path,
    environment: &carina_core::EnvironmentSnapshot,
    mode: RedactionMode,
) -> Result<String, ProbeError> {
    capture_directory(directory, environment, mode)?.complete(None)
}

fn capture_directory(
    directory: &Path,
    environment: &carina_core::EnvironmentSnapshot,
    mode: RedactionMode,
) -> Result<ProbeDocument, ProbeError> {
    let (providers, disclosure) = match mode {
        RedactionMode::RawSynthetic(factories) => {
            (OfflineProviders::Factories(factories), Disclosure::Raw)
        }
        RedactionMode::Sha256(providers) => (providers, Disclosure::Sha256),
    };
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
            Err(ProbeError::Incomplete(details)) => {
                let mut document = finish_dump(
                    PreparedProbe {
                        resources: Vec::new(),
                        compositions: Vec::new(),
                        missing: Vec::new(),
                    },
                    inputs,
                    &paths,
                    disclosure,
                )?;
                let mut missing = details.missing;
                missing.extend(document.missing);
                let mut seen = HashSet::new();
                missing.retain(|message| seen.insert(message.clone()));
                document.missing = missing;
                let mut partial: Json = serde_json::from_str(&document.dump)?;
                partial["preparation"] = json!("incomplete");
                document.dump = format!("{}\n", serde_json::to_string_pretty(&partial)?);
                Ok(document)
            }
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
                "missing schema for {}",
                resource.id
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
    let mut missing = Vec::new();
    if parsed.resources.is_empty() {
        missing.push("zero resources enumerated".to_owned());
    }
    if !parsed.deferred_for_expressions().is_empty() {
        missing.push("deferred expansion remains".to_owned());
    }
    let mut resources = Vec::new();
    for (ordinal, resource) in parsed.resources.iter().enumerate() {
        let prepared = (|| {
            if validated.context().schemas().get_for(resource).is_none() {
                return Err(ProbeError::incomplete(format!(
                    "missing schema for {}",
                    resource.id
                )));
            }
            if resource.id.identity().is_none() {
                return Err(ProbeError::incomplete(format!(
                    "unassigned resource identity: {}",
                    resource.id
                )));
            }
            let key = identity_recording::resource_key(resource).ok_or_else(|| {
                ProbeError::incomplete(format!(
                    "resource has no parser-assigned identity: {}",
                    resource.id
                ))
            })?;
            let location = identity_recording::resource_location(resource)
                .ok_or_else(|| ProbeError::incomplete("resource has no declaration location"))?;
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
                    instances.push(json!({ "instance": render_address(&call.instance, disclosure), "declaration": paths.relative(location.file().path()), "start_byte": location.span().start_byte() }));
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
                let schema = validated
                    .context()
                    .schemas()
                    .get_for(resource)
                    .expect("checked schema");
                let effective = schema
                    .schema()
                    .effective_attribute(attribute)
                    .map(|attribute| attribute.value_type());
                observe_secrets(
                    value,
                    &SecretPath(vec![SecretPathPart::Attribute(attribute.clone())]),
                    effective,
                    &mut secrets,
                    paths,
                    disclosure,
                )?;
            }
            let prefixes: BTreeMap<_, _> = resource
                .prefixes
                .iter()
                .map(|(name, value)| {
                    let value = match disclosure {
                        Disclosure::Raw => json!(value),
                        Disclosure::Sha256 => bytes(value.as_bytes(), disclosure),
                    };
                    (name, value)
                })
                .collect();
            let mut provenance = serde_json::to_value(&resource.loop_provenance)?;
            paths.loop_provenance(&mut provenance, disclosure);
            let mut resource_id = serde_json::to_value(&resource.id)?;
            resource_id["identity"] = render_address(
                resource.id.identity_str().expect("resolved identity"),
                disclosure,
            );
            Ok::<_, ProbeError>(PreparedResource {
                key,
                loop_identities: identity_recording::loop_system_identities(
                    &resource.loop_provenance,
                ),
                requires_identity: resource.binding.is_none(),
                required_modules,
                dump: json!({
                    "ordinal": ordinal,
                    "declaration": { "file": paths.relative(location.file().path()), "start_byte": span.start_byte(), "end_byte": span.end_byte(), "line": span.start_line(), "column": span.start_column() },
                    "expansion": { "instances": instances, "loop": provenance },
                    "identity_basis": basis, "resource_id": resource_id, "address": render_address(&resource.id.to_string(), disclosure),
                    "resolved_prefixes": prefixes, "secret_material": secrets,
                }),
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
    } = prepared;
    let mut claimed = vec![false; inputs.len()];
    let mut keys = HashSet::new();
    for resource in &mut resources {
        let address = format!("resource ordinal {}", resource.dump["ordinal"]);
        if !keys.insert(resource.key.clone()) {
            failures.push(format!("duplicate system resource identity for {address}"));
        }
        for module in &resource.required_modules {
            if !inputs.iter().any(|input| {
                matches!(&input.owner, InputOwner::Module(recorded) if recorded == module)
                    && input.boundary == "simhash"
                    && input.output.is_some()
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
                failures.push(format!("missing loop identity hash inputs for {address}: {identity}"));
            }
        }
        let mut rendered = Vec::new();
        let mut primary = false;
        for (index, input) in inputs.iter().enumerate() {
            let owns = match &input.owner {
                InputOwner::Resource(key) => key == &resource.key,
                InputOwner::Module(module) => module.owns(&resource.key),
                InputOwner::Loop { identity, .. } => loops.contains(identity),
                InputOwner::SecretProducer(_) | InputOwner::Unattributed => false,
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
                && input.output.is_some()
            {
                primary = true;
            }
            rendered.push(input);
        }
        if resource.requires_identity && !primary {
            failures.push(format!("missing identity hash inputs for {address}"));
        }
        resource.dump["hash_inputs"] = Json::Array(
            collapse_observations(rendered)
                .into_iter()
                .map(|input| render_input(input, paths, disclosure))
                .collect(),
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
                    && next.output.is_some()
            })
            .count();
        if input.feature_count != Some(features) {
            failures.push(format!(
                "incomplete SimHash feature recording: expected {:?}, observed {features}",
                input.feature_count
            ));
        }
    }
    let mut producers: BTreeMap<&str, Vec<&HashInput>> = BTreeMap::new();
    for (index, input) in inputs.iter().enumerate() {
        if let InputOwner::SecretProducer(producer) = input.owner {
            claimed[index] = true;
            producers.entry(producer.name()).or_default().push(input);
        }
    }
    let producers: BTreeMap<_, _> = producers
        .into_iter()
        .map(|(producer, observations)| {
            (
                producer,
                collapse_observations(observations)
                    .into_iter()
                    .map(|input| render_input(input, paths, disclosure))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
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
            InputOwner::Resource(_)
            | InputOwner::Module(_)
            | InputOwner::SecretProducer(_)
            | InputOwner::Unattributed => None,
        };
        if reason.is_none() {
            failures.push("unattributed hash inputs".to_owned());
        }
        unattributed
            .push(json!({ "reason": reason, "input": render_input(input, paths, disclosure) }));
    }
    let dump = json!({
        "format": "carina-identity-probe-v2",
        "redaction": match disclosure { Disclosure::Raw => "raw-synthetic", Disclosure::Sha256 => "sha256" },
        "resources": resources.into_iter().map(|resource| resource.dump).collect::<Vec<_>>(),
        "unattributed": unattributed,
        "secret_producers": producers,
    });
    let dump = format!("{}\n", serde_json::to_string_pretty(&dump)?);
    let mut seen = HashSet::new();
    failures.retain(|message| seen.insert(message.clone()));
    Ok(ProbeDocument {
        dump,
        missing: failures,
    })
}

fn collapse_observations<'a>(
    observations: impl IntoIterator<Item = &'a HashInput>,
) -> Vec<&'a HashInput> {
    let mut previous = HashMap::new();
    observations
        .into_iter()
        .filter(|input| previous.insert(&input.owner, *input) != Some(*input))
        .collect()
}

#[derive(Clone)]
enum SecretPathPart {
    Attribute(String),
    MapKey(String),
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
    fn render(&self, disclosure: Disclosure) -> Json {
        match disclosure {
            Disclosure::Raw => {
                let mut path = String::new();
                for part in &self.0 {
                    match part {
                        SecretPathPart::Attribute(name) | SecretPathPart::MapKey(name) => {
                            if !path.is_empty() {
                                path.push('.');
                            }
                            path.push_str(name);
                        }
                        SecretPathPart::Index(index) => {
                            use std::fmt::Write;
                            write!(path, "[{index}]").expect("String");
                        }
                    }
                }
                json!(path)
            }
            Disclosure::Sha256 => json!(
                self.0
                    .iter()
                    .map(|part| match part {
                        SecretPathPart::Attribute(name) => json!({"attribute": name}),
                        SecretPathPart::MapKey(key) =>
                            json!({"map_key": bytes(key.as_bytes(), disclosure)}),
                        SecretPathPart::Index(index) => json!({"index": index}),
                    })
                    .collect::<Vec<_>>()
            ),
        }
    }
}

fn observe_secrets(
    value: &Value,
    path: &SecretPath,
    effective: Option<carina_core::schema::EffectiveType<'_>>,
    out: &mut Vec<Json>,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<(), ProbeError> {
    match value {
        Value::Deferred(DeferredValue::Secret(payload)) => {
            for producer in [identity_recording::SecretProducer::PayloadHash, identity_recording::SecretProducer::SnapshotHash] {
                let (available, inputs) = identity_recording::record(|| match producer {
                    identity_recording::SecretProducer::PayloadHash => identity_recording::observe_secret(payload).is_ok(),
                    identity_recording::SecretProducer::SnapshotHash => identity_recording::observe_snapshot_secret(payload).is_ok(),
                    identity_recording::SecretProducer::PayloadVerification => unreachable!("probe selects hash producers"),
                });
                if available {
                    if inputs.is_empty() { return Err(ProbeError::incomplete(format!("missing {} secret material recording", producer.name()))); }
                    out.push(json!({ "path": path.render(disclosure), "producer": producer.name(), "status": "available", "inputs": collapse_observations(&inputs).into_iter().map(|input| render_input(input, paths, disclosure)).collect::<Vec<_>>() }));
                } else {
                    out.push(json!({ "path": path.render(disclosure), "producer": producer.name(), "status": "unavailable", "reason": "production producer cannot obtain material offline" }));
                }
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for (index, item) in items.iter().enumerate() {
                observe_secrets(item, &path.child(SecretPathPart::Index(index)), effective.as_ref().and_then(|ty| ty.list_element()), out, paths, disclosure)?;
            }
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            for (key, item) in entries {
                let field = effective.as_ref().filter(|ty| matches!(ty.shape(), carina_core::schema::Shape::Struct { name: _ })).and_then(|ty| {
                    ty.field_candidates_with_budget(&mut carina_core::schema::ShapeWalkBudget::new(64)).into_iter().find(|field| field.name() == key)
                });
                let (part, child_type) = match field {
                    Some(field) => (SecretPathPart::Attribute(field.name().to_owned()), Some(field.value_type())),
                    None => (SecretPathPart::MapKey(key.clone()), effective.as_ref().and_then(|ty| ty.map_value())),
                };
                observe_secrets(item, &path.child(part), child_type, out, paths, disclosure)?;
            }
        }
        _ if carina_core::value::contains_secret(value) => out.push(json!({ "path": path.render(disclosure), "status": "unavailable", "reason": "secret remains inside a deferred expression" })),
        _ => {}
    }
    Ok(())
}

fn render_address(address: &str, disclosure: Disclosure) -> Json {
    match disclosure {
        Disclosure::Raw => json!(address),
        Disclosure::Sha256 => {
            let parts = identity_recording::address_parts(address);
            if parts
                .iter()
                .all(|part| matches!(part, AddressPart::Literal(_)))
            {
                return json!(address);
            }
            json!(
                parts
                    .into_iter()
                    .map(|part| match part {
                        AddressPart::Literal(text) => json!({"literal": text}),
                        AddressPart::MapKey(key) =>
                            json!({"map_key": bytes(key.as_bytes(), disclosure)}),
                    })
                    .collect::<Vec<_>>()
            )
        }
    }
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

struct SourcePaths {
    root: carina_core::paths::PersistedPath,
}
impl SourcePaths {
    fn new(directory: &Path) -> Result<Self, ProbeError> {
        Ok(Self {
            root: carina_core::paths::PersistedPath::new(directory)?,
        })
    }
    fn relative(&self, path: &Path) -> String {
        carina_core::paths::PersistedPath::new(path)
            .expect("parser source locator")
            .lexical_relative_to(&self.root)
            .expect("source and root share an anchor")
            .to_string()
    }
    fn module_context(
        &self,
        call: &carina_core::parser::ModuleCallId,
        disclosure: Disclosure,
    ) -> Json {
        let file = self.relative(call.source_file());
        match disclosure {
            Disclosure::Raw => {
                let mut context = format!("module:<root>/{file}:{}", call.call_span().start_byte());
                if let carina_core::parser::CallExpansion::Expanded(expansion) = call.expansion() {
                    context.push(':');
                    context.push_str(expansion.as_str());
                }
                json!(context)
            }
            Disclosure::Sha256 => json!({
                "kind": "module", "file": file, "start_byte": call.call_span().start_byte(),
                "expansion": match call.expansion() {
                    carina_core::parser::CallExpansion::Direct => Json::Null,
                    carina_core::parser::CallExpansion::Expanded(expansion) => bytes(expansion.as_str().as_bytes(), disclosure),
                },
            }),
        }
    }
    fn context(&self, context: &InputContext, disclosure: Disclosure) -> Json {
        match context {
            InputContext::Resource(Some(location)) => json!(format!(
                "resource:<root>/{}:{}",
                self.relative(location.file().path()),
                location.span().start_byte()
            )),
            InputContext::Resource(None) => json!("resource:unavailable"),
            InputContext::Module(call) => self.module_context(call, disclosure),
            InputContext::SecretProducer(producer) => json!(producer.name()),
        }
    }
    fn contexts(&self, input: &HashInput, disclosure: Disclosure) -> Vec<Json> {
        match &input.owner {
            InputOwner::Resource(key) => {
                let (file, ordinal) = key.source();
                vec![json!(format!(
                    "resource:<root>/{}#{ordinal}",
                    self.relative(file)
                ))]
            }
            InputOwner::Module(key) => key
                .calls()
                .map(|call| self.module_context(call, disclosure))
                .collect(),
            InputOwner::Loop { .. } | InputOwner::SecretProducer(_) | InputOwner::Unattributed => {
                input
                    .context
                    .iter()
                    .map(|context| self.context(context, disclosure))
                    .collect()
            }
        }
    }
    fn loop_provenance(&self, value: &mut Json, disclosure: Disclosure) {
        match value {
            Json::Object(fields) => {
                let map_key = fields.get("kind").and_then(Json::as_str) == Some("key");
                for (key, value) in fields {
                    match (key.as_str(), value.as_str()) {
                        ("display_file", Some(file)) => {
                            let path = Path::new(file);
                            let path = if path.is_absolute() {
                                path.to_path_buf()
                            } else {
                                self.root.to_path_buf().join(path)
                            };
                            *value = json!(self.relative(&path));
                        }
                        ("value", Some(key)) if map_key => {
                            if matches!(disclosure, Disclosure::Sha256) {
                                *value = bytes(key.as_bytes(), disclosure);
                            }
                        }
                        (
                            "legacy" | "current_parent" | "current_prefix" | "system_identity",
                            Some(address),
                        ) => *value = render_address(address, disclosure),
                        _ => self.loop_provenance(value, disclosure),
                    }
                }
            }
            Json::Array(values) => {
                for value in values {
                    self.loop_provenance(value, disclosure);
                }
            }
            Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {}
        }
    }
}

fn bytes(value: &[u8], disclosure: Disclosure) -> Json {
    match disclosure {
        Disclosure::Raw => json!({ "hex": hex(value) }),
        Disclosure::Sha256 => {
            serde_json::to_value(identity_recording::fingerprint(value)).expect("fingerprint JSON")
        }
    }
}
fn render_input(input: &HashInput, paths: &SourcePaths, disclosure: Disclosure) -> Json {
    json!({
        "boundary": input.boundary,
        "context": paths.contexts(input, disclosure),
        "writes": input.writes.iter().map(|write| {
            let mut value = bytes(&write.bytes, disclosure);
            value.as_object_mut().expect("byte object").insert("operation".to_owned(), json!(write.operation));
            value
        }).collect::<Vec<_>>(),
        "output": input.output.as_deref().map(|output| bytes(output, disclosure)),
        "finishes": input.finishes.iter().map(|finish| json!({
            "operation": finish.operation, "after_writes": finish.after_writes, "output": bytes(&finish.output, disclosure),
        })).collect::<Vec<_>>(),
        "feature_count": input.feature_count,
    })
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("write String");
    }
    hex
}

/// Batch entry point for real directories. It has no raw-byte mode parameter.
/// Every directory is attempted; successes are written even if others fail.
pub fn probe_real_directories(
    root: &Path,
    artifacts: &Path,
    output: &Path,
    environment: &carina_core::EnvironmentSnapshot,
) -> Result<DirectoryReport, ProbeError> {
    write_directory_dumps(root, output, |directory| {
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

fn children(directory: &Path) -> Result<Vec<PathBuf>, ProbeError> {
    let mut children = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.path().is_dir() {
            children.push(entry.path());
        }
    }
    children.sort();
    if children.is_empty() {
        return Err(ProbeError::incomplete("no directories enumerated"));
    }
    Ok(children)
}

fn write_directory_dumps(
    root: &Path,
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
    let mut directories = Vec::new();
    let mut collect = |directory: &Path| match children(directory) {
        Ok(children) => children,
        Err(error) => {
            report.0.push(DirectoryResult {
                directory: directory.to_path_buf(),
                status: DirectoryStatus::Failed(error),
            });
            Vec::new()
        }
    };
    directories.extend(collect(&root.join("aws/management")));
    for registry in collect(&root.join("envs/registry")) {
        directories.extend(collect(&registry));
    }
    directories.sort();
    for directory in directories {
        let result = (|| {
            if carina_core::paths::ResolvedPath::new(&directory)?
                .crn_files()?
                .is_empty()
            {
                return Err(ProbeError::NoConfiguration);
            }
            let document = probe(&directory)?;
            let destination = output.join(
                directory
                    .strip_prefix(root)
                    .expect("walked directory below root"),
            );
            std::fs::create_dir_all(&destination)?;
            let dump_path = destination.join("identity.json");
            write_private_dump(&dump_path, document.dump.as_bytes())?;
            document.complete(Some(dump_path)).map(|_| ())
        })();
        let status = match result {
            Ok(()) => DirectoryStatus::Ok,
            Err(ProbeError::NoConfiguration) => DirectoryStatus::Skipped,
            Err(error) => DirectoryStatus::Failed(error),
        };
        report.0.push(DirectoryResult { directory, status });
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

fn write_private_dump(path: &Path, bytes: &[u8]) -> Result<(), ProbeError> {
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
    file.write_all(bytes)?;
    Ok(())
}
