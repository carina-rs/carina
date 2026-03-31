//! Integration test: spawn mock provider process and verify CRUD operations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Once;

use carina_plugin_host::process::ProviderProcess;
use carina_provider_protocol::methods;

static BUILD_ONCE: Once = Once::new();

fn build_mock_process() -> PathBuf {
    BUILD_ONCE.call_once(|| {
        let status = Command::new("cargo")
            .args(["build", "-p", "carina-provider-mock-process"])
            .status()
            .expect("Failed to run cargo build");
        assert!(status.success(), "Failed to build mock-process provider");
    });

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let binary = workspace_root.join("target/debug/carina-provider-mock-process");
    assert!(binary.exists(), "Binary not found: {}", binary.display());
    binary
}

#[test]
fn test_process_provider_info() {
    let binary = build_mock_process();
    let mut process = ProviderProcess::spawn(&binary).expect("Failed to spawn");

    let result: methods::ProviderInfoResult = process
        .call("provider_info", &serde_json::json!({}))
        .expect("provider_info failed");

    assert_eq!(result.info.name, "mock");
    assert_eq!(result.info.display_name, "Mock Provider (Process)");

    process.shutdown();
}

#[test]
fn test_process_provider_crud_cycle() {
    let binary = build_mock_process();
    let mut process = ProviderProcess::spawn(&binary).expect("Failed to spawn");

    // Initialize
    let _: methods::InitializeResult = process
        .call(
            "initialize",
            &methods::InitializeParams {
                attributes: HashMap::new(),
            },
        )
        .expect("initialize failed");

    // Read — should not exist
    let read_result: methods::ReadResult = process
        .call(
            "read",
            &methods::ReadParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: None,
            },
        )
        .expect("read failed");
    assert!(!read_result.state.exists);

    // Create
    let create_result: methods::CreateResult = process
        .call(
            "create",
            &methods::CreateParams {
                resource: carina_provider_protocol::types::Resource {
                    id: carina_provider_protocol::types::ResourceId {
                        provider: "mock".into(),
                        resource_type: "test.resource".into(),
                        name: "hello".into(),
                    },
                    attributes: HashMap::from([(
                        "value".into(),
                        carina_provider_protocol::types::Value::String("world".into()),
                    )]),
                    lifecycle: Default::default(),
                },
            },
        )
        .expect("create failed");
    assert!(create_result.state.exists);
    assert_eq!(create_result.state.identifier, Some("mock-id".into()));

    // Read — should exist now
    let read_result2: methods::ReadResult = process
        .call(
            "read",
            &methods::ReadParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: Some("mock-id".into()),
            },
        )
        .expect("read failed");
    assert!(read_result2.state.exists);

    // Delete
    let delete_result: methods::DeleteResult = process
        .call(
            "delete",
            &methods::DeleteParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: "mock-id".into(),
                lifecycle: Default::default(),
            },
        )
        .expect("delete failed");
    assert!(delete_result.ok);

    // Read — should not exist after delete
    let read_result3: methods::ReadResult = process
        .call(
            "read",
            &methods::ReadParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: None,
            },
        )
        .expect("read failed");
    assert!(!read_result3.state.exists);

    process.shutdown();
}
