//! Offline identity observations for comparing #4400 PR heads.
//!
//! Synthetic callers explicitly choose raw bytes or fingerprints. The real
//! directory entry point only writes SHA-256 and byte lengths, never raw bytes.
//! No provider runtime, backend, state read, plan, or download is involved.

use super::{LoadedProviderFactory, ProviderLoad, ProviderNoticeSink, WiringContext};
use crate::error::AppError;
use carina_core::identity_recording::{self, HashInput, InputOwner, ModuleKey, ResourceKey};
use carina_core::resource::{ConcreteValue, DeferredValue, ResourceIdentity, Value};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

pub enum OfflineProviders {
    Factories(Vec<Box<dyn carina_core::provider::ProviderFactory>>),
    /// Flat provider WASMs, an installed project, or its `.carina/providers`.
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
    /// Association failures include the dump, including its unattributed inputs.
    #[error("incomplete offline identity probe: {0}")]
    Incomplete(String),
    #[error(transparent)]
    DirectoryFailures(#[from] DirectoryFailures),
}
impl From<AppError> for ProbeError {
    fn from(error: AppError) -> Self {
        Self::Application(Box::new(error))
    }
}

#[derive(Debug)]
pub struct DirectoryFailure {
    pub directory: PathBuf,
    pub error: ProbeError,
}
#[derive(Debug)]
pub struct DirectoryFailures(Vec<DirectoryFailure>);
impl DirectoryFailures {
    pub fn failures(&self) -> &[DirectoryFailure] {
        &self.0
    }
}
impl std::fmt::Display for DirectoryFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} offline identity probe directories failed",
            self.0.len()
        )?;
        for failure in &self.0 {
            write!(f, "\n{}: {}", failure.directory.display(), failure.error)?;
        }
        Ok(())
    }
}
impl std::error::Error for DirectoryFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0
            .first()
            .map(|failure| &failure.error as &(dyn std::error::Error + 'static))
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
}

/// Run synchronously outside an async runtime. Provider schemas and environment
/// are explicit inputs. Byte disclosure is a required, closed API choice.
pub fn probe_directory(
    directory: &Path,
    environment: &carina_core::EnvironmentSnapshot,
    mode: RedactionMode,
) -> Result<String, ProbeError> {
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
        finish_dump(prepared?, inputs, &paths, disclosure)
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
        return Err(ProbeError::Incomplete("zero resources enumerated".into()));
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
            return Err(ProbeError::Incomplete(format!(
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
    if parsed.resources.is_empty() {
        return Err(ProbeError::Incomplete("zero resources enumerated".into()));
    }
    if !parsed.deferred_for_expressions().is_empty() {
        return Err(ProbeError::Incomplete("deferred expansion remains".into()));
    }
    let mut resources = Vec::new();
    for (ordinal, resource) in parsed.resources.iter().enumerate() {
        if validated.context().schemas().get_for(resource).is_none() {
            return Err(ProbeError::Incomplete(format!(
                "missing schema for {}",
                resource.id
            )));
        }
        if resource.id.identity().is_none() {
            return Err(ProbeError::Incomplete(format!(
                "unassigned resource identity: {}",
                resource.id
            )));
        }
        let key = identity_recording::resource_key(resource).ok_or_else(|| {
            ProbeError::Incomplete(format!(
                "resource has no parser-assigned identity: {}",
                resource.id
            ))
        })?;
        let location = identity_recording::resource_location(resource)
            .ok_or_else(|| ProbeError::Incomplete("resource has no declaration location".into()))?;
        let span = location.span();
        let mut instances = Vec::new();
        let mut required_modules = Vec::new();
        if let Some(module) = resource
            .module_source
            .as_ref()
            .and_then(|source| source.module_call())
        {
            let keys = ModuleKey::prefixes(module.module_call_path().calls().map(|call| &call.id));
            for (call, key) in module.module_call_path().calls().zip(keys) {
                if call.binding.is_none() {
                    required_modules.push(key);
                }
                let location = call.id.declaration_location();
                instances.push(json!({ "instance": call.instance, "declaration": paths.relative(location.file().path()), "start_byte": location.span().start_byte() }));
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
                    return Err(ProbeError::Incomplete(
                        "incomplete declared identity".into(),
                    ));
                }
                None => {
                    return Err(ProbeError::Incomplete(
                        "identity basis has no schema".into(),
                    ));
                }
            }
        };
        let mut secrets = Vec::new();
        for (attribute, value) in &resource.attributes {
            observe_secrets(value, attribute, &mut secrets, paths, disclosure)?;
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
        paths.loop_provenance(&mut provenance);
        resources.push(PreparedResource {
            key,
            loop_identities: identity_recording::loop_system_identities(&resource.loop_provenance),
            requires_identity: resource.binding.is_none(),
            required_modules,
            dump: json!({
                "ordinal": ordinal,
                "declaration": { "file": paths.relative(location.file().path()), "start_byte": span.start_byte(), "end_byte": span.end_byte(), "line": span.start_line(), "column": span.start_column() },
                "expansion": { "instances": instances, "loop": provenance },
                "identity_basis": basis, "resource_id": resource.id, "address": resource.id.to_string(),
                "resolved_prefixes": prefixes, "secret_material": secrets,
            }),
        });
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
    })
}

fn finish_dump(
    prepared: PreparedProbe,
    inputs: Vec<HashInput>,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<String, ProbeError> {
    let PreparedProbe {
        mut resources,
        compositions,
    } = prepared;
    let mut claimed = vec![false; inputs.len()];
    let mut failures = Vec::new();
    let mut keys = HashSet::new();
    for resource in &mut resources {
        let address = resource.dump["address"]
            .as_str()
            .expect("resource address")
            .to_owned();
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
                InputOwner::Unattributed => false,
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
            rendered.push(render_input(input, paths, disclosure));
        }
        if resource.requires_identity && !primary {
            failures.push(format!("missing identity hash inputs for {address}"));
        }
        resource.dump["hash_inputs"] = Json::Array(rendered);
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
    let mut unattributed = Vec::new();
    for (input, claimed) in inputs.iter().zip(claimed) {
        if claimed {
            continue;
        }
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
            _ => None,
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
    });
    let dump = format!("{}\n", serde_json::to_string_pretty(&dump)?);
    if failures.is_empty() {
        Ok(dump)
    } else {
        Err(ProbeError::Incomplete(format!(
            "{}\n{dump}",
            failures.join("; ")
        )))
    }
}

fn observe_secrets(
    value: &Value,
    path: &str,
    out: &mut Vec<Json>,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<(), ProbeError> {
    match value {
        Value::Deferred(DeferredValue::Secret(payload)) => {
            let (result, inputs) = identity_recording::record(|| identity_recording::observe_secret(payload));
            match result {
                Ok(()) => {
                    if inputs.is_empty() { return Err(ProbeError::Incomplete(format!("missing secret hash material recording for {path}"))); }
                    out.push(json!({ "path": path, "status": "available", "inputs": inputs.iter().map(|input| render_input(input, paths, disclosure)).collect::<Vec<_>>() }));
                }
                Err(_) => out.push(json!({ "path": path, "status": "unavailable", "reason": "production encoder cannot obtain material offline" })),
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => for (index, item) in items.iter().enumerate() { observe_secrets(item, &format!("{path}[{index}]"), out, paths, disclosure)?; },
        Value::Concrete(ConcreteValue::Map(entries)) => for (key, item) in entries { observe_secrets(item, &format!("{path}.{key}"), out, paths, disclosure)?; },
        _ if carina_core::value::contains_secret(value) => out.push(json!({ "path": path, "status": "unavailable", "reason": "secret remains inside a deferred expression" })),
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
        return Err(ProbeError::Incomplete(format!(
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
                return Err(ProbeError::Incomplete(format!(
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
        return Err(ProbeError::Incomplete(
            "no offline provider schemas loaded".into(),
        ));
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
    fn contexts(&self, input: &HashInput) -> Vec<String> {
        match &input.owner {
            InputOwner::Resource(key) => {
                let (file, ordinal) = key.source();
                vec![format!("resource:<root>/{}#{ordinal}", self.relative(file))]
            }
            InputOwner::Module(key) => key
                .calls()
                .map(|call| {
                    let mut context = format!(
                        "module:<root>/{}:{}",
                        self.relative(call.source_file()),
                        call.call_span().start_byte()
                    );
                    if let carina_core::parser::CallExpansion::Expanded(expansion) =
                        call.expansion()
                    {
                        context.push(':');
                        context.push_str(expansion.as_str());
                    }
                    context
                })
                .collect(),
            _ => input
                .context
                .iter()
                .map(|context| context.replace(&format!("{}/", self.root.display()), "<root>/"))
                .collect(),
        }
    }
    fn loop_provenance(&self, value: &mut Json) {
        match value {
            Json::Object(fields) => {
                for (key, value) in fields {
                    if key == "display_file" {
                        if let Some(file) = value.as_str() {
                            let path = Path::new(file);
                            let path = if path.is_absolute() {
                                path.to_path_buf()
                            } else {
                                self.root.to_path_buf().join(path)
                            };
                            *value = json!(self.relative(&path));
                        }
                    } else {
                        self.loop_provenance(value);
                    }
                }
            }
            Json::Array(values) => {
                for value in values {
                    self.loop_provenance(value);
                }
            }
            _ => {}
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
        "context": paths.contexts(input),
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
) -> Result<(), ProbeError> {
    write_directory_dumps(root, output, |directory| {
        probe_directory(
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
        return Err(ProbeError::Incomplete("no directories enumerated".into()));
    }
    Ok(children)
}

fn write_directory_dumps(
    root: &Path,
    output: &Path,
    mut probe: impl FnMut(&Path) -> Result<String, ProbeError>,
) -> Result<(), ProbeError> {
    let mut failures = Vec::new();
    let mut directories = Vec::new();
    let mut collect = |directory: &Path| match children(directory) {
        Ok(children) => children,
        Err(error) => {
            failures.push(DirectoryFailure {
                directory: directory.to_path_buf(),
                error,
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
        let result = probe(&directory).and_then(|dump| {
            let destination = output.join(
                directory
                    .strip_prefix(root)
                    .expect("walked directory below root"),
            );
            std::fs::create_dir_all(&destination)?;
            write_private_dump(&destination.join("identity.json"), dump.as_bytes())
        });
        if let Err(error) = result {
            failures.push(DirectoryFailure { directory, error });
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(DirectoryFailures(failures).into())
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
