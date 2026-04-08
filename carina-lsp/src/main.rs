use std::collections::HashMap;
use std::path::Path;

use carina_core::parser::{ProviderConfig, ProviderContext};
use carina_core::provider::ProviderFactory;
use tower_lsp::{LspService, Server};

use carina_lsp::Backend;

/// Build provider factories from discovered provider configs.
///
/// For each provider config with a `source`, resolves the cached WASM binary
/// and loads it as a `WasmProviderFactory`. Providers without source or with
/// missing/invalid WASM binaries are skipped with a log warning.
fn build_factories(providers: &[ProviderConfig], base_dir: &Path) -> Vec<Box<dyn ProviderFactory>> {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();

    for config in providers {
        let source = match &config.source {
            Some(s) => s,
            None => continue,
        };

        let binary_path = if let Some(path) = source.strip_prefix("file://") {
            std::path::PathBuf::from(path)
        } else if source.starts_with("github.com/") {
            match carina_provider_resolver::resolve_single_config(base_dir, config) {
                Ok(path) => path,
                Err(e) => {
                    log::warn!("LSP: failed to resolve provider '{}': {}", config.name, e);
                    continue;
                }
            }
        } else {
            log::warn!(
                "LSP: unsupported source format for provider '{}': {}",
                config.name,
                source
            );
            continue;
        };

        if !carina_provider_resolver::is_wasm_provider(&binary_path) {
            log::warn!(
                "LSP: provider '{}' is not a WASM component: {}",
                config.name,
                binary_path.display()
            );
            continue;
        }

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
            }
            Err(e) => {
                log::warn!("LSP: failed to load WASM provider '{}': {}", config.name, e);
            }
        }
    }

    factories
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        let cwd = std::env::current_dir().ok();
        let factories = if let Some(ref dir) = cwd {
            let providers = carina_lsp::workspace::discover_providers(dir);
            if providers.is_empty() {
                vec![]
            } else {
                build_factories(&providers, dir)
            }
        } else {
            vec![]
        };

        log::info!("LSP: loaded {} provider factories", factories.len());

        let provider_context = ProviderContext {
            decryptor: None,
            validators: HashMap::new(),
        };
        Backend::new(client, factories, provider_context)
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
