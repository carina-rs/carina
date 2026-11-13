//! Stable, deliberately small facade for comparing #4400 PR heads.
//!
//! This runs directory loading and the production canonical-pair preparation
//! through identity assignment. It does not prepare a plan, initialize a
//! provider, open a backend, resolve upstream state values, or download plugins.
//! Dumps contain secret hash material, including plaintext, by design.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use carina_core::identity_recording::{self, HashInput};
use carina_core::resource::{ConcreteValue, DeferredValue, ResourceIdentity, Value};
use serde_json::{Value as Json, json};

use super::{LoadedProviderFactory, ProviderLoad, ProviderNoticeSink, WiringContext};
use crate::error::AppError;

/// Metadata providers for an entirely offline run.
pub enum OfflineProviders {
    /// In-process schema factories. Provider initialization is never called.
    Factories(Vec<Box<dyn carina_core::provider::ProviderFactory>>),
    /// Either a directory of `<provider>.wasm` (also accepting Cargo's
    /// `carina_provider_<provider>.wasm` / `carina-provider-<provider>.wasm`),
    /// or a project containing installed `.carina/providers` and its lock file.
    /// The installed `.carina/providers` directory itself is also accepted.
    ArtifactDirectory(PathBuf),
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
    Incomplete(String),
}

impl From<AppError> for ProbeError {
    fn from(error: AppError) -> Self {
        Self::Application(Box::new(error))
    }
}

struct PreparedResource {
    dump: Json,
    contexts: Vec<String>,
    loop_identities: Vec<ResourceIdentity>,
}

/// Return a deterministic JSON dump using only local source and schema bytes.
/// Call synchronously, outside an async runtime. Declaration paths are relative
/// to `directory`; hash bytes are never normalized or re-encoded. Environment
/// inputs are owned by the caller, just as at the normal executable boundary.
pub fn probe_directory(
    directory: &Path,
    providers: OfflineProviders,
    environment: &carina_core::EnvironmentSnapshot,
) -> Result<String, ProbeError> {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    // block_on's root future stays on this thread, keeping the recorder scoped
    // across schema-only loading's block_in_place calls.
    executor.block_on(async {
        let (result, inputs) =
            identity_recording::record(|| prepare(directory, providers, environment));
        let mut resources = result?;
        for resource in &mut resources {
            let mut selected: Vec<&HashInput> = inputs
                .iter()
                .filter(|input| {
                    input
                        .context
                        .iter()
                        .any(|context| resource.contexts.contains(context))
                        || (input.boundary == "loop-system-identity"
                            && input.output.as_ref().is_some_and(|digest| {
                                let identity = format!("_loop_{}", hex(&digest[..8]));
                                resource
                                    .loop_identities
                                    .iter()
                                    .any(|source| source.as_str() == identity)
                            }))
                })
                .collect();
            // Include an earlier loop parent when module scoping hashed that
            // parent's identity. Follow recorded results, never recompute them.
            loop {
                let parents: Vec<_> = inputs
                    .iter()
                    .filter(|candidate| {
                        candidate.boundary == "loop-system-identity"
                            && !selected
                                .iter()
                                .any(|existing| std::ptr::eq(*existing, *candidate))
                            && candidate.output.as_ref().is_some_and(|digest| {
                                let parent = format!("_loop_{}", hex(&digest[..8]));
                                selected.iter().any(|child| {
                                    child
                                        .writes
                                        .iter()
                                        .any(|write| write.bytes == parent.as_bytes())
                                })
                            })
                    })
                    .collect();
                if parents.is_empty() {
                    break;
                }
                selected.extend(parents);
            }
            let mut rendered = selected
                .into_iter()
                .map(|input| render_input(input, directory))
                .collect::<Vec<_>>();
            rendered.sort_by_key(Json::to_string);
            resource.dump["hash_inputs"] = Json::Array(rendered);
        }
        let dump = json!({
            "format": "carina-identity-probe-v1",
            "resources": resources.into_iter().map(|resource| resource.dump).collect::<Vec<_>>(),
        });
        Ok(format!("{}\n", serde_json::to_string_pretty(&dump)?))
    })
}

fn prepare(
    directory: &Path,
    providers: OfflineProviders,
    environment: &carina_core::EnvironmentSnapshot,
) -> Result<Vec<PreparedResource>, ProbeError> {
    let directory = carina_core::paths::ResolvedPath::new(directory)?;
    let runtime =
        crate::test_support::TestCommandRuntime::new().with_environment(environment.clone());
    let loaded = carina_core::config_loader::load_configuration_with_config(
        &directory,
        &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
        &carina_core::schema::SchemaRegistry::new(),
    )
    .map_err(AppError::from)?;
    let factories = match providers {
        OfflineProviders::Factories(factories) => LoadedProviderFactory::for_test(factories),
        OfflineProviders::ArtifactDirectory(artifacts) => {
            load_artifacts(&runtime, &directory, &loaded.parsed.providers, &artifacts)?
        }
    };
    let context = WiringContext::new(&runtime, factories, &loaded.parsed.providers)?;
    // Use the same paired preparation as plan/apply. Admission and state-based
    // work are deliberately omitted; every expansion and identity error still
    // fails. This also keeps the probe usable during the admission migration.
    let validated = ProviderLoad {
        context,
        load_errors: HashMap::new(),
    }
    .validate_config_pair(
        loaded.parsed,
        loaded.unresolved_parsed,
        &directory,
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
    let source_root = directory.source_label();
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
        let location = identity_recording::resource_location(resource)
            .ok_or_else(|| ProbeError::Incomplete("resource has no declaration location".into()))?;
        let span = location.span();
        let mut resource_contexts = vec![format!("resource:{ordinal}")];
        let mut instances = Vec::new();
        if let Some(module) = resource
            .module_source
            .as_ref()
            .and_then(|source| source.module_call())
        {
            for call in module.module_call_path().calls() {
                let location = call.id.declaration_location();
                resource_contexts.push(identity_recording::module_context_key(&call.id));
                instances.push(json!({
                    "instance": call.instance,
                    "declaration": relative_path(location.file().path(), source_root.as_path()),
                    "start_byte": location.span().start_byte(),
                }));
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
                ) => "incomplete-declared-identity",
                None => {
                    return Err(ProbeError::Incomplete(
                        "identity basis has no schema".into(),
                    ));
                }
            }
        };
        let mut secrets = Vec::new();
        for (attribute, value) in &resource.attributes {
            observe_secrets(value, attribute, &mut secrets, source_root.as_path());
        }
        let dump = json!({
            "ordinal": ordinal,
            "declaration": {
                "file": relative_path(location.file().path(), source_root.as_path()),
                "start_byte": span.start_byte(), "end_byte": span.end_byte(),
                "line": span.start_line(), "column": span.start_column(),
            },
            "expansion": { "instances": instances, "loop": resource.loop_provenance },
            "identity_basis": basis,
            "resource_id": resource.id,
            "address": resource.id.to_string(),
            "resolved_prefixes": resource.prefixes.iter().collect::<BTreeMap<_, _>>(),
            "secret_material": secrets,
        });
        resources.push(PreparedResource {
            dump,
            contexts: resource_contexts,
            loop_identities: identity_recording::loop_system_identities(&resource.loop_provenance),
        });
    }
    Ok(resources)
}

fn observe_secrets(value: &Value, path: &str, out: &mut Vec<Json>, directory: &Path) {
    match value {
        Value::Deferred(DeferredValue::Secret(payload)) => {
            let (result, inputs) = identity_recording::record(|| identity_recording::observe_secret(payload));
            out.push(match result {
                Ok(()) => json!({ "path": path, "status": "available", "inputs": inputs.iter().map(|input| render_input(input, directory)).collect::<Vec<_>>() }),
                Err(_) => json!({ "path": path, "status": "unavailable", "reason": "production encoder cannot obtain material offline" }),
            });
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for (index, item) in items.iter().enumerate() {
                observe_secrets(item, &format!("{path}[{index}]"), out, directory);
            }
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            for (key, item) in entries {
                observe_secrets(item, &format!("{path}.{key}"), out, directory);
            }
        }
        _ if carina_core::value::contains_secret(value) => out.push(json!({
            "path": path, "status": "unavailable", "reason": "secret remains inside a deferred expression",
        })),
        _ => {}
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

fn relative_path(path: &Path, directory: &Path) -> String {
    path.strip_prefix(directory)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn render_input(input: &HashInput, directory: &Path) -> Json {
    json!({
        "boundary": input.boundary,
        "context": input.context.iter().map(|context| context.replace(&format!("{}/", directory.display()), "<root>/")).collect::<Vec<_>>(),
        "writes": input.writes.iter().map(|write| json!({ "operation": write.operation, "hex": hex(&write.bytes) })).collect::<Vec<_>>(),
        "output": input.output.as_deref().map(hex),
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("write to String");
    }
    hex
}
