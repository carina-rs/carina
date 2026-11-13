//! Offline identity observations for comparing #4400 PR heads.
//!
//! Synthetic callers explicitly choose raw bytes or fingerprints. Real mode
//! fingerprints every complete identity/address and other user-derived string
//! with SHA-256 plus byte length. Plaintext requires an explicit allow-list kind:
//! FixedLiteral, RelativePath, SchemaOrTypeName, AttributeName, SourceBindingName.
//! Typed renderers select every field; serialized structures never pass through.
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
//! secret material. Secrets not known offline have `status: unavailable`.
//! Deferred directories are covered by plan parity (`scripts/validate-parity.py`),
//! not by this probe. Partial dumps identify exactly which iterable sites defer.
//! Application diagnostics are reported by error kind only. Run `carina validate`
//! on the directory for details; those diagnostics may quote authored values.
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

/// Only these five explicit classifications can disclose plaintext in real mode.
#[derive(Clone, Copy)]
pub enum DisclosureKind {
    FixedLiteral,
    RelativePath,
    SchemaOrTypeName,
    AttributeName,
    SourceBindingName,
    Fingerprint,
}
impl Disclosure {
    fn disclose(self, kind: DisclosureKind, value: &str) -> Json {
        match (self, kind) {
            (Self::Raw, _)
            | (
                Self::Sha256,
                DisclosureKind::FixedLiteral
                | DisclosureKind::RelativePath
                | DisclosureKind::SchemaOrTypeName
                | DisclosureKind::AttributeName
                | DisclosureKind::SourceBindingName,
            ) => json!(value),
            (Self::Sha256, DisclosureKind::Fingerprint) => bytes(value.as_bytes(), self),
        }
    }
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
    pub file: String,
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
    dump: String,
    missing: Vec<String>,
    deferred: Vec<DeferredSite>,
}
impl ProbeDocument {
    fn complete(self, dump_path: Option<PathBuf>) -> Result<ProbeOutcome, ProbeError> {
        if self.missing.is_empty() {
            Ok(ProbeOutcome {
                dump: self.dump,
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
    dump: Json,
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
                paths.declaration(identity_recording::resource_location(resource))?
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
                file: paths.relative(location.file().path()),
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
            let declaration = paths.declaration(Some(location))?;
            if validated.context().schemas().get_for(resource).is_none() {
                return Err(ProbeError::incomplete(format!(
                    "missing schema at {declaration}"
                )));
            }
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
                    instances.push(json!({ "instance": disclosure.disclose(DisclosureKind::Fingerprint, &call.instance), "declaration": disclosure.disclose(DisclosureKind::RelativePath, &paths.relative(location.file().path())), "start_byte": location.span().start_byte() }));
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
                    (
                        disclosure
                            .disclose(DisclosureKind::AttributeName, name)
                            .as_str()
                            .expect("attribute name")
                            .to_owned(),
                        disclosure.disclose(DisclosureKind::Fingerprint, value),
                    )
                })
                .collect();
            let provenance = paths.loop_provenance(&resource.loop_provenance, disclosure);
            let mut resource_id = json!({
                "provider": disclosure.disclose(DisclosureKind::SchemaOrTypeName, &resource.id.provider),
                "resource_type": disclosure.disclose(DisclosureKind::SchemaOrTypeName, &resource.id.resource_type),
                "identity": disclosure.disclose(DisclosureKind::Fingerprint, resource.id.identity_str().expect("resolved identity")),
            });
            if let Some(binding) = &resource.id.provider_instance {
                resource_id["provider_instance"] =
                    disclosure.disclose(DisclosureKind::Fingerprint, binding);
            }
            Ok::<_, ProbeError>(PreparedResource {
                key,
                loop_identities: identity_recording::loop_system_identities(
                    &resource.loop_provenance,
                ),
                requires_identity: resource.binding.is_none(),
                required_modules,
                dump: json!({
                    "ordinal": ordinal,
                    "declaration": { "file": disclosure.disclose(DisclosureKind::RelativePath, &paths.relative(location.file().path())), "start_byte": span.start_byte(), "end_byte": span.end_byte(), "line": span.start_line(), "column": span.start_column() },
                    "expansion": { "instances": instances, "loop": provenance },
                    "identity_basis": disclosure.disclose(DisclosureKind::FixedLiteral, basis), "resource_id": resource_id, "address": disclosure.disclose(DisclosureKind::Fingerprint, &resource.id.to_string()),
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
        let address = format!(
            "{}:{}",
            resource.dump["declaration"]["file"]
                .as_str()
                .expect("declaration file"),
            resource.dump["declaration"]["start_byte"]
        );
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
        unattributed
            .push(json!({ "reason": reason.map(|reason| disclosure.disclose(DisclosureKind::FixedLiteral, reason)), "input": render_input(input, paths, disclosure) }));
    }
    let dump = json!({
        "format": disclosure.disclose(DisclosureKind::FixedLiteral, "carina-identity-probe-v2"),
        "redaction": disclosure.disclose(DisclosureKind::FixedLiteral, match disclosure { Disclosure::Raw => "raw-synthetic", Disclosure::Sha256 => "sha256" }),
        "resources": resources.into_iter().map(|resource| resource.dump).collect::<Vec<_>>(),
        "unattributed": unattributed,
        "status": { "kind": disclosure.disclose(DisclosureKind::FixedLiteral, if !failures.is_empty() { "failed" }
            else if deferred.is_empty() { "ok" } else { "partial" }), "deferred": deferred.iter().map(|site| json!({"file": disclosure.disclose(DisclosureKind::RelativePath, &site.file), "span": site.span})).collect::<Vec<_>>() },
    });
    let dump = format!("{}\n", serde_json::to_string_pretty(&dump)?);
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
                        SecretPathPart::Attribute(name) => json!({"attribute": disclosure.disclose(DisclosureKind::AttributeName, name)}),
                        SecretPathPart::MapKey(key) =>
                            json!({"map_key": disclosure.disclose(DisclosureKind::Fingerprint, key)}),
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
            let known = value.knowledge(&carina_core::resource::knowledge::KnowledgeContext::validation()).ok().and_then(|proof| proof.into_classification().into_known().ok()).is_some();
            let (available, inputs) = if known { identity_recording::record(|| payload.hash(None).is_ok()) } else { (false, Vec::new()) };
            let producer = disclosure.disclose(DisclosureKind::FixedLiteral, "secret-payload-hash");
            if available {
                if inputs.is_empty() { return Err(ProbeError::incomplete("missing secret material recording")); }
                out.push(json!({ "path": path.render(disclosure), "producer": producer, "status": "available", "inputs": collapse_observations(&inputs).into_iter().map(|input| render_input(input, paths, disclosure)).collect::<Vec<_>>() }));
            } else {
                out.push(json!({ "path": path.render(disclosure), "producer": producer, "status": "unavailable", "reason": "secret is not known offline" }));
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
    fn declaration(
        &self,
        location: Option<&carina_core::parser::DeclarationLocation>,
    ) -> Result<String, ProbeError> {
        let location = location
            .ok_or_else(|| ProbeError::incomplete("resource has no declaration location"))?;
        Ok(format!(
            "{}:{}",
            self.relative(location.file().path()),
            location.span().start_byte()
        ))
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
                disclosure.disclose(DisclosureKind::RelativePath, &context)
            }
            Disclosure::Sha256 => json!({
                "kind": disclosure.disclose(DisclosureKind::FixedLiteral, "module"), "file": disclosure.disclose(DisclosureKind::RelativePath, &file), "start_byte": call.call_span().start_byte(),
                "expansion": match call.expansion() {
                    carina_core::parser::CallExpansion::Direct => Json::Null,
                    carina_core::parser::CallExpansion::Expanded(expansion) => disclosure.disclose(DisclosureKind::Fingerprint, expansion.as_str()),
                },
            }),
        }
    }
    fn context(&self, context: &InputContext, disclosure: Disclosure) -> Json {
        match context {
            InputContext::Resource(Some(location)) => disclosure.disclose(
                DisclosureKind::RelativePath,
                &format!(
                    "resource:<root>/{}:{}",
                    self.relative(location.file().path()),
                    location.span().start_byte()
                ),
            ),
            InputContext::Resource(None) => {
                disclosure.disclose(DisclosureKind::FixedLiteral, "resource:unavailable")
            }
            InputContext::Module(call) => self.module_context(call, disclosure),
            InputContext::PayloadHash => {
                disclosure.disclose(DisclosureKind::FixedLiteral, "secret-payload-hash")
            }
        }
    }
    fn contexts(&self, input: &HashInput, disclosure: Disclosure) -> Vec<Json> {
        match &input.owner {
            InputOwner::Resource(key) => {
                let (file, ordinal) = key.source();
                vec![disclosure.disclose(
                    DisclosureKind::RelativePath,
                    &format!("resource:<root>/{}#{ordinal}", self.relative(file)),
                )]
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
        }
    }
    fn loop_provenance(&self, provenance: &LoopProvenance, disclosure: Disclosure) -> Json {
        let (direct, modules) = provenance.recording_parts();
        let mut out = serde_json::Map::new();
        if let Some(origin) = direct {
            out.insert("direct".into(), self.loop_origin(origin, disclosure));
        }
        let modules: Vec<_> = modules.map(|(origin, prefix)| json!({
            "origin": self.loop_origin(origin, disclosure),
            "current_prefix": disclosure.disclose(DisclosureKind::Fingerprint, prefix.as_str()),
        })).collect();
        if !modules.is_empty() {
            out.insert("module_prefixes".into(), json!(modules));
        }
        Json::Object(out)
    }
    fn loop_origin(&self, origin: &LoopElementOrigin, disclosure: Disclosure) -> Json {
        let (source, legacy, parent) = origin.recording_parts();
        let (source_kind, identity) = source.recording_parts();
        let kind = match origin.kind() {
            LoopOriginKind::For { element } => json!({
                "kind": disclosure.disclose(DisclosureKind::FixedLiteral, "for"),
                "element": match element {
                    ForLoopElement::Index(index) => json!({"kind": disclosure.disclose(DisclosureKind::FixedLiteral, "index"), "value": index}),
                    ForLoopElement::Key(key) => json!({"kind": disclosure.disclose(DisclosureKind::FixedLiteral, "key"), "value": disclosure.disclose(DisclosureKind::Fingerprint, key)}),
                },
            }),
            LoopOriginKind::If => {
                json!({"kind": disclosure.disclose(DisclosureKind::FixedLiteral, "if")})
            }
        };
        let file = Path::new(source.file());
        let file = if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.root.to_path_buf().join(file)
        };
        let mut out = json!({
            "kind": kind,
            "source": {
                "display_file": disclosure.disclose(DisclosureKind::RelativePath, &self.relative(&file)),
                "line": source.line(), "column": source.column(),
                "kind": disclosure.disclose(DisclosureKind::FixedLiteral, match source_kind { LoopKind::For => "for", LoopKind::If => "if" }),
                "system_identity": disclosure.disclose(DisclosureKind::Fingerprint, identity.as_str()),
            },
            "legacy": disclosure.disclose(DisclosureKind::Fingerprint, legacy),
        });
        if let Some(parent) = parent {
            out["current_parent"] =
                disclosure.disclose(DisclosureKind::Fingerprint, parent.as_str());
        }
        out
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
        "boundary": disclosure.disclose(DisclosureKind::FixedLiteral, input.boundary),
        "context": paths.contexts(input, disclosure),
        "writes": input.writes.iter().map(|write| {
            let mut value = bytes(&write.bytes, disclosure);
            value.as_object_mut().expect("byte object").insert("operation".to_owned(), disclosure.disclose(DisclosureKind::FixedLiteral, write.operation));
            value
        }).collect::<Vec<_>>(),
        "finishes": input.finishes.iter().map(|finish| json!({
            "operation": disclosure.disclose(DisclosureKind::FixedLiteral, finish.operation), "after_writes": finish.after_writes, "output": bytes(&finish.output, disclosure),
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
            write_private_dump(&dump_path, document.dump.as_bytes())?;
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
