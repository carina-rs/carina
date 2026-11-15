use std::process::Command;

#[path = "support/conversion_totality.rs"]
mod round5_fixtures;
#[path = "support/conversion_twins.rs"]
mod twins;

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

#[test]
fn round5_generated_no_as_transport_twins_keep_mains_ordinary_result() {
    use round5_fixtures::*;
    let directory = tempfile::tempdir().unwrap();
    let count = twins::emit_twins(directory.path());
    eprintln!("{count} no-as twins: {}", directory.path().display());
    let mut checked = 0;
    for position in POSITIONS {
        for known in [true, false] {
            let path = directory
                .path()
                .join("transport")
                .join(position.label())
                .join(if known { "known" } else { "unknown" })
                .join("valid");
            let json = carina_cli::commands::validate::validate_diagnostics_with_factories(
                &carina_core::paths::ResolvedPath::new(path).unwrap(),
                carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
                Default::default(),
            )
            .json;
            assert_eq!(
                json["status"],
                if known { "ok" } else { "error" },
                "{position:?}/{known}: {json}"
            );
            assert!(!json.to_string().contains("conversion_"), "{json}");
            checked += 1;
        }
    }
    assert_eq!(checked, POSITIONS.len() * 2);
}
