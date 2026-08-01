use std::collections::HashMap;
use std::path::{Path, PathBuf};

use carina_core::parser::{ProviderConfig, ProviderContext};
use carina_core::provider::ProviderFactory;
use tower_lsp::{LspService, Server};

use carina_lsp::Backend;
use carina_lsp::backend::FactoryBuildResult;

#[derive(Debug, PartialEq, Eq)]
enum InstallRejection {
    MissingSource,
    UnsupportedSource { source: String },
    ResolutionFailed { message: String },
    NotWasmComponent { path: PathBuf },
}

impl std::fmt::Display for InstallRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallRejection::MissingSource => f.write_str(
                "no source configured. Add `source = 'github.com/...'` to the provider block.",
            ),
            InstallRejection::UnsupportedSource { source } => {
                write!(f, "unsupported source format: {source}")
            }
            InstallRejection::ResolutionFailed { message } => f.write_str(message),
            InstallRejection::NotWasmComponent { path } => {
                write!(f, "not a WASM component: {}", path.display())
            }
        }
    }
}

/// Resolve the on-disk artifact the LSP would load for `config`, or retain the
/// exact reason it rejected the source. The rejection enum prevents the load
/// path from having to re-run filesystem resolution to reconstruct a reason.
///
/// Shared by `build_factories` (at load time) and the drift-poll prober (at
/// poll time) so both sides agree on what "installed" means. In particular:
/// for `file://` sources, "installed" is the source file itself existing and
/// being a WASM component — not whether a copy landed in `.carina/…/file/`.
/// That matters because `build_factories` loads the direct path, so the
/// drift poll must observe that same path to detect its deletion.
fn resolve_install(
    source_dir: &Path,
    config: &ProviderConfig,
) -> Result<carina_provider_resolver::InstalledProvider, InstallRejection> {
    let source = config
        .source
        .as_deref()
        .ok_or(InstallRejection::MissingSource)?;
    let installed = if source.starts_with("file://") {
        carina_provider_resolver::find_file_provider_source(config)
    } else if source.starts_with("github.com/") {
        carina_provider_resolver::find_installed_provider(source_dir, config)
    } else {
        return Err(InstallRejection::UnsupportedSource {
            source: source.to_string(),
        });
    }
    .map_err(|message| InstallRejection::ResolutionFailed { message })?;
    if !carina_provider_resolver::is_wasm_provider(installed.path()) {
        return Err(InstallRejection::NotWasmComponent {
            path: installed.path().to_path_buf(),
        });
    }
    // Both resolver functions only construct `InstalledProvider` after their
    // selected path exists, so a second `.exists()` check here was unreachable.
    Ok(installed)
}

/// Build provider factories from discovered provider configs.
/// Each entry is (source_directory, provider_config) so providers are installed
/// in the directory containing the `.crn` file, not at the workspace root.
fn build_factories(providers: &[(PathBuf, ProviderConfig)]) -> FactoryBuildResult {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut fingerprint: Vec<(String, bool)> = Vec::with_capacity(providers.len());

    for (source_dir, config) in providers {
        let installed = match resolve_install(source_dir, config) {
            Ok(installed) => installed,
            Err(rejection) => {
                // Named instances inherit the kind default's source. Their
                // missing source is deliberate; every other rejection is a
                // diagnostic produced directly from this one resolution.
                if config.is_default || rejection != InstallRejection::MissingSource {
                    errors.insert(config.name.clone(), rejection.to_string());
                }
                fingerprint.push((config.name.clone(), false));
                continue;
            }
        };

        match tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(installed.load_with(carina_plugin_host::WasmProviderFactory::new))
        }) {
            Ok(factory) => {
                log::info!(
                    "LSP: loaded provider '{}' from {}",
                    config.name,
                    installed.path().display()
                );
                factories.push(Box::new(factory));
                fingerprint.push((config.name.clone(), true));
            }
            Err(error) => {
                // Final LSP-diagnostic boundary. The structured host chain is
                // rendered first and resolver provenance follows it.
                errors.insert(config.name.clone(), error.to_string());
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
            std::sync::Arc::new(|dir, cfg| resolve_install(dir, cfg).is_ok());

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

    #[test]
    fn direct_file_install_retains_non_lock_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm = tmp.path().join("local-provider.wasm");
        std::fs::write(&wasm, b"fixture").unwrap();
        let providers = cfg(
            "local",
            Some(&format!("file://{}", wasm.display())),
            true,
            None,
        );

        let installed = resolve_install(&providers.0, &providers.1)
            .expect("direct file source should be observed by the LSP");
        assert_eq!(installed.path(), wasm);
        let rendered = installed.with_load_error("load failed").to_string();
        assert!(rendered.contains("provider resolved from file://"));
        assert!(rendered.contains("not controlled by carina-providers.lock"));
        assert!(!rendered.contains("carina init --upgrade"));
    }

    #[test]
    fn github_installed_non_wasm_artifact_produces_a_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "github.com/example/native-provider";
        let version = "1.2.3";
        let native_path = carina_provider_resolver::cache_path(tmp.path(), source, version);
        std::fs::create_dir_all(native_path.parent().unwrap()).unwrap();
        std::fs::write(&native_path, b"native provider fixture").unwrap();

        let mut lock = carina_provider_resolver::LockFile::default();
        lock.upsert(carina_provider_resolver::LockEntry {
            name: "native".into(),
            source: source.into(),
            kind: carina_provider_resolver::LockEntryKind::Version {
                version: version.into(),
                constraint: None,
            },
            sha256: "fixture".into(),
            registry: None,
        });
        lock.save(&tmp.path().join("carina-providers.lock"))
            .unwrap();

        let (_, config) = cfg("native", Some(source), true, None);
        let providers = vec![(tmp.path().to_path_buf(), config)];
        let (_factories, errors, fingerprint) = build_factories(&providers);

        let diagnostic = errors
            .get("native")
            .expect("an installed non-WASM artifact must never be silently dropped");
        assert!(diagnostic.contains("not a WASM component"));
        assert!(diagnostic.contains(&native_path.display().to_string()));
        assert_eq!(fingerprint, vec![("native".to_string(), false)]);
    }
}
