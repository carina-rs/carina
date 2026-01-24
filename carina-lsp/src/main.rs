use tower_lsp::{LspService, Server};

mod backend;
mod completion;
mod diagnostics;
mod document;
mod hover;
mod semantic_tokens;

use backend::Backend;

#[tokio::main]
async fn main() {
    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
