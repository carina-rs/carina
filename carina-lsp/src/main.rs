use std::collections::HashMap;
use std::path::{Path, PathBuf};

use carina_core::parser::{ProviderConfig, ProviderContext};
use carina_core::provider::ProviderFactory;
use tower_lsp::{LspService, Server};

use carina_lsp::Backend;
use carina_lsp::backend::FactoryBuildResult;

/// Resolve the on-disk path the LSP would load for `config`, if any. `None`
/// means the provider is not currently installed for the LSP's purposes.
///
/// Shared by `build_factories` (at load time) and the drift-poll prober (at
/// poll time) so both sides agree on what "installed" means. In particular:
/// for `file://` sources, "installed" is the source file itself existing and
/// being a WASM component — not whether a copy landed in `.carina/…/file/`.
/// That matters because `build_factories` loads the direct path, so the
/// drift poll must observe that same path to detect its deletion.
fn resolve_install(source_dir: &Path, config: &ProviderConfig) -> Option<PathBuf> {
    let source = config.source.as_deref()?;
    let binary_path = if let Some(path) = source.strip_prefix("file://") {
        PathBuf::from(path)
    } else if source.starts_with("github.com/") {
        carina_provider_resolver::find_installed_provider(source_dir, config).ok()?
    } else {
        return None;
    };
    if !carina_provider_resolver::is_wasm_provider(&binary_path) {
        return None;
    }
    if !binary_path.exists() {
        return None;
    }
    Some(binary_path)
}

/// Build provider factories from discovered provider configs.
/// Each entry is (source_directory, provider_config) so providers are installed
/// in the directory containing the `.crn` file, not at the workspace root.
fn build_factories(providers: &[(PathBuf, ProviderConfig)]) -> FactoryBuildResult {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut fingerprint: Vec<(String, bool)> = Vec::with_capacity(providers.len());

    for (source_dir, config) in providers {
        let source = match &config.source {
            Some(s) => s,
            None => {
                // Named provider instances (`let <name> = provider <kind>
                // { ... }`) inherit `source` from the kind's default;
                // the parser forbids them from setting it themselves.
                // Only the kind default's deliberate absence of
                // `source` is a real user error (carina#3023). The
                // `fingerprint` push stays outside this gate so
                // every config in `providers` produces exactly one
                // entry — `probe_install_fingerprint` iterates the
                // same slice unconditionally, and the LSP's
                // drift-poll compares the two fingerprint vectors
                // for equality.
                if config.is_default {
                    errors.insert(
                        config.name.clone(),
                        "no source configured. Add `source = 'github.com/...'` to the provider block.".to_string(),
                    );
                }
                fingerprint.push((config.name.clone(), false));
                continue;
            }
        };

        let binary_path = match resolve_install(source_dir, config) {
            Some(path) => path,
            None => {
                // Build the same error wording the previous code path produced,
                // so surfaced diagnostics match what users saw before.
                if source.starts_with("file://") {
                    let stripped = source.strip_prefix("file://").unwrap_or(source);
                    let p = PathBuf::from(stripped);
                    if !p.exists() {
                        errors.insert(config.name.clone(), format!("file not found: {}", stripped));
                    } else if !carina_provider_resolver::is_wasm_provider(&p) {
                        errors.insert(
                            config.name.clone(),
                            format!("not a WASM component: {}", p.display()),
                        );
                    }
                } else if source.starts_with("github.com/") {
                    if let Err(e) =
                        carina_provider_resolver::find_installed_provider(source_dir, config)
                    {
                        errors.insert(config.name.clone(), e);
                    }
                } else {
                    errors.insert(
                        config.name.clone(),
                        format!("unsupported source format: {}", source),
                    );
                }
                fingerprint.push((config.name.clone(), false));
                continue;
            }
        };

        match tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                carina_plugin_host::WasmProviderFactory::new(binary_path.clone()),
            )
        }) {
            Ok(factory) => {
                log::info!(
                    "LSP: loaded provider '{}' from {}",
                    config.name,
                    binary_path.display()
                );
                factories.push(Box::new(factory));
                fingerprint.push((config.name.clone(), true));
            }
            Err(e) => {
                errors.insert(config.name.clone(), format!("failed to load WASM: {}", e));
                // Factory failed to load even though the path resolved; treat
                // as "not installed" from the LSP's perspective so the next
                // poll can notice if the user replaces the WASM.
                fingerprint.push((config.name.clone(), false));
            }
        }
    }

    (factories, errors, fingerprint)
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        let provider_context = ProviderContext {
            decryptor: None,
            validators: HashMap::new(),
            custom_type_validator: None,
            schema_types: Default::default(),
            resource_types: Default::default(),
            // Schemas load asynchronously after LSP initialize; the
            // strict carina#3239 parser check is enabled inside
            // `DiagnosticEngine::new` once schemas are present.
            customs_loaded: false,
        };

        // Pass factory builder callback — actual WASM loading happens asynchronously
        // after initialize, not during server construction.
        let factory_builder: carina_lsp::backend::FactoryBuilder =
            std::sync::Arc::new(build_factories);

        // Provider install prober: used by the drift poller to notice when
        // `<project>/.carina/` is deleted mid-session. Shares `resolve_install`
        // with `build_factories` so "installed" means the same thing to both —
        // the snapshot captured at build time and the drift-poll observation
        // describe the same filesystem state.
        let install_prober: carina_lsp::backend::ProviderInstallProber =
            std::sync::Arc::new(|dir, cfg| resolve_install(dir, cfg).is_some());

        Backend::with_install_prober(
            client,
            provider_context,
            Some(factory_builder),
            Some(install_prober),
        )
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::parser::ProviderConfig;
    use indexmap::IndexMap;
    use std::path::PathBuf;

    fn cfg(
        name: &str,
        source: Option<&str>,
        is_default: bool,
        binding: Option<&str>,
    ) -> (PathBuf, ProviderConfig) {
        (
            PathBuf::from("/tmp"),
            ProviderConfig {
                name: name.to_string(),
                attributes: IndexMap::new(),
                default_tags: IndexMap::new(),
                source: source.map(String::from),
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: binding.map(String::from),
                is_default,
            },
        )
    }

    /// carina#3023: when a named provider instance sits beside the
    /// kind default, `build_factories` must produce a fingerprint
    /// entry for *every* config — same length as the input — so the
    /// LSP's drift-poll comparison against `probe_install_fingerprint`
    /// (which iterates every config unconditionally) keeps agreeing
    /// when nothing changed. Previously the fingerprint push was
    /// gated behind the missing-source error path; when we silenced
    /// that error for named instances, the push got silenced with
    /// it and the poll detected fake drift every tick.
    #[test]
    fn build_factories_fingerprint_length_matches_configs_length() {
        let providers = vec![
            cfg("aws", Some("file:///nonexistent/fake.wasm"), true, None),
            cfg("aws", None, false, Some("us")),
        ];
        let (_factories, errors, fingerprint) = build_factories(&providers);
        assert_eq!(
            fingerprint.len(),
            providers.len(),
            "fingerprint must emit one entry per config (including named instances); \
             otherwise the drift-poll mismatch causes a perpetual rebuild loop. carina#3023."
        );
        // Sanity: named instance does not surface the kind-level
        // "no source configured" error, since the parser forbids it
        // from setting `source` in the first place.
        assert!(
            !errors
                .get("aws")
                .is_some_and(|m| m.contains("no source configured")),
            "named instance must not trigger the kind-level missing-source diagnostic. \
             carina#3023. errors: {:?}",
            errors
        );
    }
}
