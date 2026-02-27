use carina_core::provider::ProviderFactory;
use carina_provider_aws::AwsProviderFactory;
use carina_provider_awscc::AwsccProviderFactory;
use tower_lsp::{LspService, Server};

use carina_lsp::Backend;

#[tokio::main]
async fn main() {
    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        let factories: Vec<Box<dyn ProviderFactory>> =
            vec![Box::new(AwsProviderFactory), Box::new(AwsccProviderFactory)];
        Backend::new(client, factories)
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
