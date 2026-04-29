mod support;

use std::time::Duration;

use support::TestClient;

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_struct_field_completion() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_struct.crn";
    let text = r#"awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress {

    }
}"#;

    client.open_document(uri, text).await;

    // Small delay to let the server process didOpen
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Request completion inside the security_group_ingress block (line 3)
    let response = client._request_completion(uri, 3, 8).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");

    // Verify struct field completions
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"ip_protocol"),
        "Should have ip_protocol completion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"from_port"),
        "Should have from_port completion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"to_port"),
        "Should have to_port completion. Got: {:?}",
        labels
    );

    // Verify they are FIELD kind (5 in LSP spec)
    for item in items {
        let label = item["label"].as_str().unwrap_or("");
        if label == "ip_protocol" || label == "from_port" || label == "to_port" {
            assert_eq!(
                item["kind"].as_u64(),
                Some(5), // CompletionItemKind::FIELD
                "{} should have FIELD kind",
                label
            );
        }
    }

    client.shutdown().await;
}

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_resource_attribute_completion() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_attr.crn";
    let text = "aws.s3.Bucket {\n    \n}";

    client.open_document(uri, text).await;

    // Small delay to let the server process didOpen
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Request completion inside the block (line 1, after indentation)
    let response = client._request_completion(uri, 1, 4).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");

    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"bucket"),
        "Should have 'bucket' attribute completion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"versioning_status"),
        "Should have 'versioning_status' attribute completion. Got: {:?}",
        labels
    );

    client.shutdown().await;
}
