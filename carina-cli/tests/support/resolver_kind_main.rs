//! Stable cell comparison helpers; refreshing is explicit in the ignored recorder.
use serde_json::Value;
use std::path::{Path, PathBuf};
pub const FIRST_RECORDED_ON: &str = "dedb7bcebd10f6b9a6b2d4a7cd6c8b318f8b2ccd";
pub fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/authored_reference_cells.json")
}
pub fn normalize_prefix(value: Value, root: &Path, token: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(root.to_str().unwrap(), token)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| normalize_prefix(v, root, token))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(k, v)| (k, normalize_prefix(v, root, token)))
                .collect(),
        ),
        value => value,
    }
}
pub fn json_difference(expected: &Value, actual: &Value) -> Option<String> {
    fn difference(expected: &Value, actual: &Value, path: &str) -> Option<String> {
        match (expected, actual) {
            (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
                .iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (a, b))| difference(a, b, &format!("{path}/{i}"))),
            (Value::Object(a), Value::Object(b)) if a.keys().eq(b.keys()) => a
                .iter()
                .find_map(|(k, a)| difference(a, &b[k], &format!("{path}/{k}"))),
            _ if expected == actual => None,
            _ => Some(format!("{path}: expected {expected}, got {actual}")),
        }
    }
    difference(expected, actual, "")
}
pub fn assert_json_eq(expected: &Value, actual: &Value, context: &str) {
    if let Some(diff) = json_difference(expected, actual) {
        panic!("{context}: {diff}");
    }
}
// Failure evidence only, outside the worktree; never an update mode.
pub fn save_actual(label: &str, bytes: &[u8]) -> PathBuf {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new()
        .prefix(&format!("pr1-actual-{}-", label.replace('/', "__")))
        .suffix(".json")
        .tempfile()
        .unwrap();
    file.write_all(bytes).unwrap();
    file.keep().unwrap().1
}
