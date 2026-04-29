mod support;

use std::time::Duration;

use support::TestClient;

/// Regression for #2196 (empty-candidate side).
///
/// End-to-end test: a leaf config dir (mirror of
/// `infra/aws/management/github-oidc/` — multi-file, no sibling module
/// dirs) asks for completion inside `use { source = '.|' }`. The LSP
/// must return the `./` and `../` navigation anchors so the user has
/// a starting point; prior to the fix this call returned an empty list.
#[tokio::test]
async fn test_use_source_path_completion_offers_anchors_in_leaf_dir() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    // Mirror the real infra shape: a leaf config dir with main/providers/backend,
    // no `modules/` subtree of its own.
    let tmp = tempfile::tempdir().unwrap();
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("providers.crn"),
        "provider aws {\n  region = \"us-east-1\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        leaf.join("backend.crn"),
        "backend {\n  type = \"local\"\n}\n",
    )
    .unwrap();

    // Multi-line shape — mirrors what the user types. The opening quote
    // has no matching closing quote yet (this is the normal mid-typing
    // state); the context detector expects that. A closed `'...'` shape
    // is also handled elsewhere in the code path but isn't what we
    // exercise here.
    let main_text = "let shared = use {\n  source = '.";
    let main_path = leaf.join("main.crn");
    std::fs::write(&main_path, main_text).unwrap();

    let uri = format!("file://{}", main_path.display());
    client.open_document(&uri, main_text).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cursor at end of line 1 (after the `.`).
    let last_line = main_text.lines().next_back().unwrap();
    let character = last_line.chars().count() as u32;
    let response = client._request_completion(&uri, 1, character).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"./"),
        "Should offer './' anchor. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"../"),
        "Should offer '../' anchor. Got: {:?}",
        labels
    );

    client.shutdown().await;
}

/// End-to-end regression guard for #2196 (context-detection side): a full
/// LSP session where the editor opens a `.crn` with a multi-line
/// `use { source = '…' }` and requests completion at the cursor inside
/// `source`. Exercises the real JSON-RPC path, not only the internal
/// `CompletionProvider::complete`.
#[tokio::test]
async fn test_use_source_path_completion_multiline() {
    // Lay out a real workspace on disk so the LSP can walk it from the
    // opened document's directory.
    let tmp = tempfile::tempdir().unwrap();
    let modules_dir = tmp.path().join("modules");
    std::fs::create_dir_all(modules_dir.join("network")).unwrap();
    std::fs::create_dir_all(modules_dir.join("shared")).unwrap();

    let crn_path = tmp.path().join("main.crn");
    let text = "let network = use {\n  source = './modules/\n}";
    std::fs::write(&crn_path, text).unwrap();

    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = format!("file://{}", crn_path.display());
    client.open_document(&uri, text).await;

    // Small delay to let the server process didOpen.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cursor at the end of the `source = './modules/` line (line 1).
    let second_line = text.lines().nth(1).unwrap();
    let character = second_line.chars().count() as u32;
    let response = client._request_completion(&uri, 1, character).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");

    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"network/"),
        "Should suggest 'network/' directory from './modules/'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"shared/"),
        "Should suggest 'shared/' directory from './modules/'. Got: {:?}",
        labels
    );

    client.shutdown().await;
}

/// Regression for #2200: attribute-name position inside `use { ... }` must
/// offer `source`. Multi-file directory fixture mirrors the real
/// `infra/aws/management/<leaf>/` shape per CLAUDE.md's directory-scoped
/// rule, so we're exercising the handler against the same shape users
/// actually edit.
#[tokio::test]
async fn test_use_block_offers_source_attribute() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let tmp = tempfile::tempdir().unwrap();
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("providers.crn"),
        "provider aws {\n  region = \"us-east-1\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        leaf.join("backend.crn"),
        "backend {\n  type = \"local\"\n}\n",
    )
    .unwrap();

    // Multi-line mid-typing shape. Cursor on the empty line inside the block.
    let main_text = "let shared = use {\n\n}\n";
    let main_path = leaf.join("main.crn");
    std::fs::write(&main_path, main_text).unwrap();

    let uri = format!("file://{}", main_path.display());
    client.open_document(&uri, main_text).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Line 1, col 0 — inside the block, attribute-name position.
    let response = client._request_completion(&uri, 1, 0).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert_eq!(
        labels,
        vec!["source"],
        "use block should offer exactly one attribute ('source'). Got: {:?}",
        labels
    );

    client.shutdown().await;
}
