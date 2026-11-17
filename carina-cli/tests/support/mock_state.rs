use std::{fs, path::Path};

/// Find an applied object's provider identifier through its Carina state row.
pub fn identifier(project: &Path, identity: &str) -> String {
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(project.join("carina.state.json")).expect("read applied Carina state"),
    )
    .expect("decode applied Carina state");
    state["resources"]
        .as_array()
        .expect("applied resources")
        .iter()
        .find(|row| row["identity"] == identity)
        .unwrap_or_else(|| panic!("missing applied resource {identity}: {state:#}"))
        .pointer("/current/identifier")
        .and_then(serde_json::Value::as_str)
        .expect("applied resource has a provider identifier")
        .to_owned()
}
