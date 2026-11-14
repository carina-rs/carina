use carina_core::{EnvironmentSnapshot, parser::ProviderContext};
use carina_lsp::Backend;
use carina_lsp::factories::{build_factories, install_prober, trusted_local_grants_from_env};
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    env_logger::init();

    // Provider loading needs user-owned storage, but the rest of the language
    // server remains useful when that storage cannot be resolved.
    #[expect(clippy::disallowed_methods)]
    let user_dirs = carina_core::UserDirs::from_real_home().map_err(|error| {
        let message = error.to_string();
        log::error!("LSP provider loading disabled: {message}");
        message
    });
    let user_dirs = std::sync::Arc::new(user_dirs);

    #[expect(clippy::disallowed_methods)]
    let environment = EnvironmentSnapshot::capture_process();
    let trusted_local_grants =
        trusted_local_grants_from_env(environment.get("CARINA_TRUST_LOCAL_PROVIDER"), |error| {
            log::error!("Ignoring invalid CARINA_TRUST_LOCAL_PROVIDER: {error}");
        });
    let trusted_local_grants = std::sync::Arc::new(trusted_local_grants);

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(move |client| {
        let provider_context = ProviderContext::new(environment.clone());

        // Pass factory builder callback — actual WASM loading happens asynchronously
        // after initialize, not during server construction.
        let trusted_local_grants = std::sync::Arc::clone(&trusted_local_grants);
        let user_dirs = std::sync::Arc::clone(&user_dirs);
        let factory_builder: carina_lsp::backend::FactoryBuilder =
            std::sync::Arc::new(move |providers| {
                build_factories(
                    user_dirs.as_ref().as_ref().map_err(String::as_str),
                    providers,
                    &trusted_local_grants,
                )
            });

        // Provider install prober: used by the drift poller to notice selected
        // artifact or lock-file metadata changes. Factory construction still
        // performs SHA256 verification.
        Backend::with_install_prober(
            client,
            provider_context,
            Some(factory_builder),
            Some(install_prober()),
        )
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
