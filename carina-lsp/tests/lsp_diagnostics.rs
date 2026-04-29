mod support;

use std::time::Duration;

use support::TestClient;

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_diagnostics_for_unknown_struct_field() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_diag.crn";
    let text = r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.SecurityGroup {
    name = "test-sg"
    group_description = "Test security group"
    security_group_ingress {
        ip_protocol = "tcp"
        unknown_field = "bad"
    }
}"#;

    client.open_document(uri, text).await;

    // Read publishDiagnostics notification
    let notification = client
        ._read_notification("textDocument/publishDiagnostics", Duration::from_secs(5))
        .await
        .expect("Should receive publishDiagnostics notification");

    let diagnostics = notification["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array");

    // Find the unknown_field diagnostic
    let has_unknown_field = diagnostics.iter().any(|d| {
        d["message"]
            .as_str()
            .is_some_and(|m| m.contains("unknown_field"))
    });

    assert!(
        has_unknown_field,
        "Should have diagnostic about unknown_field. Got: {:?}",
        diagnostics
            .iter()
            .filter_map(|d| d["message"].as_str())
            .collect::<Vec<_>>()
    );

    client.shutdown().await;
}

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_diagnostics_for_exclusive_required_attrs() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_exclusive.crn";
    // vpc_gateway_attachment requires exactly one of internet_gateway_id or vpn_gateway_id,
    // but here neither is specified.
    let text = r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc_gateway_attachment {
    vpc_id = "vpc-12345678"
}"#;

    client.open_document(uri, text).await;

    let notification = client
        ._read_notification("textDocument/publishDiagnostics", Duration::from_secs(5))
        .await
        .expect("Should receive publishDiagnostics notification");

    let diagnostics = notification["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array");

    let has_exclusive_error = diagnostics.iter().any(|d| {
        d["message"]
            .as_str()
            .is_some_and(|m| m.contains("Exactly one of"))
    });

    assert!(
        has_exclusive_error,
        "Should have diagnostic about exclusive required attrs. Got: {:?}",
        diagnostics
            .iter()
            .filter_map(|d| d["message"].as_str())
            .collect::<Vec<_>>()
    );

    client.shutdown().await;
}
