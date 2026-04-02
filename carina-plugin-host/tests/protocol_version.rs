//! Integration test: spawn the mock provider binary and verify protocol version handshake.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Once;

static BUILD_ONCE: Once = Once::new();

fn mock_binary_path() -> PathBuf {
    BUILD_ONCE.call_once(|| {
        let status = Command::new("cargo")
            .args([
                "build",
                "-p",
                "carina-provider-mock",
                "--bin",
                "carina-provider-mock",
            ])
            .status()
            .expect("Failed to run cargo build");
        assert!(status.success(), "Failed to build mock provider");
    });

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let binary = workspace_root.join("target/debug/carina-provider-mock");
    assert!(
        binary.exists(),
        "mock provider binary not found at {}",
        binary.display()
    );
    binary
}

#[test]
fn test_spawn_mock_provider_succeeds() {
    let path = mock_binary_path();
    let mut process = carina_plugin_host::process::ProviderProcess::spawn(&path)
        .expect("Should spawn mock provider with matching protocol version");
    process.shutdown();
}
