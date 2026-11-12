use std::process::Command;

#[test]
fn conversion_no_as_directories_match_main_validate_json() {
    let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../carina-core/tests/fixtures/conversion_no_as_parity.json"
    ))
    .unwrap();
    for case in cases {
        let directory = tempfile::tempdir().unwrap();
        for (file, source) in case["files"].as_object().unwrap() {
            let path = directory.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source.as_str().unwrap()).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(directory.path())
            .args(["validate", ".", "--json"])
            .output()
            .unwrap();
        assert!(
            output.stderr.is_empty(),
            "{}: {}",
            case["name"],
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}
