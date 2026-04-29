mod support;

use support::TestClient;

#[tokio::test]
async fn test_initialize_returns_completion_provider() {
    let mut client = TestClient::new().await;
    let response = client.initialize().await;

    let capabilities = &response["result"]["capabilities"];

    // Verify completionProvider is present
    assert!(
        capabilities.get("completionProvider").is_some(),
        "Server should advertise completionProvider"
    );

    // Verify trigger characters include "."
    let trigger_chars = capabilities["completionProvider"]["triggerCharacters"]
        .as_array()
        .expect("triggerCharacters should be an array");

    let has_dot = trigger_chars.iter().any(|v| v.as_str() == Some("."));
    assert!(has_dot, "Trigger characters should include '.'");

    client.shutdown().await;
}
