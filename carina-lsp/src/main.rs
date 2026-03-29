use carina_core::parser::ProviderContext;
use carina_core::provider::ProviderFactory;
use carina_provider_aws::AwsProviderFactory;
use carina_provider_awscc::AwsccProviderFactory;
use carina_provider_awscc::schemas::awscc_types::awscc_validators;
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
        let provider_context = ProviderContext {
            decryptor: None,
            validators: awscc_validators(),
        };
        Backend::new(client, factories, provider_context)
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
