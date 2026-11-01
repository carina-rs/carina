mod support;

use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
};
use support::fixture::write_fixture;

use support::TestClient;

fn completion_schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    let ingress = AttributeType::list(AttributeType::struct_(
        "Ingress".to_string(),
        vec![
            StructField::optional("ip_protocol", AttributeType::string()).build(),
            StructField::optional("from_port", AttributeType::int()).build(),
            StructField::optional("to_port", AttributeType::int()).build(),
        ],
    ));
    schemas.insert(
        "awscc",
        ResourceSchema::new("ec2.SecurityGroup")
            .attribute(
                AttributeSchema::optional("group_description", AttributeType::string()).build(),
            )
            .attribute(
                AttributeSchema::optional("security_group_ingress", ingress)
                    .with_block_name("security_group_ingress")
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "aws",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::optional("bucket", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("versioning_status", AttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    schemas
}

#[tokio::test]
async fn test_struct_field_completion() {
    let text = r#"awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress {

    }
}"#;

    let fixture = write_fixture(&[
        ("main.crn", text),
        ("providers.crn", "provider awscc {}\nprovider aws {}\n"),
    ]);
    let mut client = TestClient::with_schemas(fixture.path(), completion_schemas()).await;
    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn")).unwrap();
    let uri = uri.as_str();
    client.open_document(uri, text).await;
    client.wait_for_diagnostics(uri).await;

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

#[tokio::test]
async fn test_resource_attribute_completion() {
    let text = "aws.s3.Bucket {\n    \n}";

    let fixture = write_fixture(&[
        ("main.crn", text),
        ("providers.crn", "provider awscc {}\nprovider aws {}\n"),
    ]);
    let mut client = TestClient::with_schemas(fixture.path(), completion_schemas()).await;
    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn")).unwrap();
    let uri = uri.as_str();
    client.open_document(uri, text).await;
    client.wait_for_diagnostics(uri).await;

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
