use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::parser::{ProviderConfig, ProviderContext};
use carina_core::provider::ProviderFactory;
use tower_lsp::{LspService, Server};

use carina_lsp::Backend;
use carina_lsp::backend::FactoryBuildResult;

/// Build provider factories from discovered provider configs.
/// Each entry is (source_directory, provider_config) so providers are installed
/// in the directory containing the `.crn` file, not at the workspace root.
fn build_factories(providers: &[(PathBuf, ProviderConfig)]) -> FactoryBuildResult {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
    let mut errors: HashMap<String, String> = HashMap::new();

    for (source_dir, config) in providers {
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
            // LSP uses find_installed_provider (no download) instead of
            // resolve_single_config to avoid filesystem side effects.
            match carina_provider_resolver::find_installed_provider(source_dir, config) {
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
            custom_type_validator: None,
        };

        // Pass factory builder callback — actual WASM loading happens asynchronously
        // after initialize, not during server construction.
        let factory_builder: carina_lsp::backend::FactoryBuilder =
            std::sync::Arc::new(build_factories);

        Backend::new(client, provider_context, Some(factory_builder))
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
