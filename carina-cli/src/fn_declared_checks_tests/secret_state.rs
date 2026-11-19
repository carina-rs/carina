use super::runtime_checks::{assert_no_secret, secret_case};
use super::*;

fn secret_hashes(directory: &Directory) -> BTreeMap<String, serde_json::Value> {
    let text = std::fs::read_to_string(directory.path().join("state.json")).unwrap();
    assert_no_secret(&text, "secret state");
    let state: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut hashes = BTreeMap::new();
    for (kind, attribute) in [("Thing", "plain"), ("Sink", "value")] {
        let rows: Vec<_> = state["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["resource_type"] == kind)
            .collect();
        assert_eq!(rows.len(), 1, "one {kind} row");
        let value = &rows[0]["current"]["attributes"][attribute];
        assert!(
            value
                .get("$secret")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "{kind}.{attribute} must remain hashed"
        );
        hashes.insert(format!("{kind}.{attribute}"), value.clone());
    }
    hashes
}

#[tokio::test]
async fn refresh_preserves_hashes_for_function_authored_secret_state() {
    let mut seed = secret_case(true);
    seed.child = seed
        .child
        .map(|source| source.replace("): test.Name", "): String"));
    let directory = Directory::new(&seed);
    directory.apply().await.unwrap();
    directory.fixture.operations.lock().unwrap().clear();
    let before = secret_hashes(&directory);

    let result = directory.refresh().await;

    assert_no_secret(&rendered(&result), "refresh diagnostic");
    result.unwrap();
    assert_eq!(secret_hashes(&directory), before);

    assert_eq!(directory.fixture.count("create", "Thing"), 0);
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn primitive_function_result_preserves_secrets_during_apply_and_refresh() {
    for root in [false, true] {
        let mut case = secret_case(true);
        if root {
            case.main = case
                .child
                .take()
                .unwrap()
                .lines()
                .filter(|line| !line.starts_with("arguments "))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let source = case.child.as_mut().unwrap_or(&mut case.main);
        *source = source.replace("): test.Name", "): String");
        let directory = Directory::new(&case);

        let applied = directory.apply().await;

        assert_no_secret(&rendered(&applied), "apply diagnostic");
        applied.unwrap();
        let before = secret_hashes(&directory);

        let planned = directory.plan(None).await;
        let refreshed = directory.refresh().await;

        assert_no_secret(&rendered(&planned), "plan diagnostic");
        assert_no_secret(&rendered(&refreshed), "refresh diagnostic");
        assert!(!planned.unwrap());
        refreshed.unwrap();
        assert_eq!(secret_hashes(&directory), before);

        assert_eq!(directory.fixture.count("create", "Thing"), 1);
        assert_eq!(directory.fixture.count("create", "Sink"), 1);
        directory.assert_unlocked().await;
    }
}
