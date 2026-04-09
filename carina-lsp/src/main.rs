use std::collections::HashMap;
use std::path::Path;

use carina_core::parser::{ProviderConfig, ProviderContext};
use carina_core::provider::ProviderFactory;
use tower_lsp::{LspService, Server};

use carina_lsp::Backend;
use carina_lsp::backend::FactoryBuildResult;

/// Build provider factories from discovered provider configs.
/// Returns loaded factories and a map of provider name -> error reason for failures.
fn build_factories(providers: &[ProviderConfig], base_dir: &Path) -> FactoryBuildResult {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
    let mut errors: HashMap<String, String> = HashMap::new();

    for config in providers {
        let source = match &config.source {
            Some(s) => s,
            None => {
                errors.insert(
                    config.name.clone(),
                    "no source configured. Add `source = 'github.com/...'` to the provider block."
                        .to_string(),
                );
                continue;
            }
        };

        let binary_path = if let Some(path) = source.strip_prefix("file://") {
            std::path::PathBuf::from(path)
        } else if source.starts_with("github.com/") {
            match carina_provider_resolver::resolve_single_config(base_dir, config) {
                Ok(path) => path,
                Err(e) => {
                    errors.insert(config.name.clone(), e);
                    continue;
                }
            }
        } else {
            errors.insert(
                config.name.clone(),
                format!("unsupported source format: {}", source),
            );
            continue;
        };

        if !carina_provider_resolver::is_wasm_provider(&binary_path) {
            errors.insert(
                config.name.clone(),
                format!("not a WASM component: {}", binary_path.display()),
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
                errors.insert(config.name.clone(), format!("failed to load WASM: {}", e));
            }
        }
    }

    (factories, errors)
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
        };

        // Pass factory builder callback — actual WASM loading happens asynchronously
        // after initialize, not during server construction.
        let factory_builder: carina_lsp::backend::FactoryBuilder =
            std::sync::Arc::new(build_factories);

        Backend::new(client, provider_context, Some(factory_builder))
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
