use std::process::Command;

#[path = "support/conversion_totality.rs"]
mod round4_fixtures;

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
fn round4_generated_no_as_twins_keep_mains_vacuous_conversion_result() {
    use round4_fixtures::*;
    let directory = tempfile::tempdir().unwrap();
    let count = emit_twins(directory.path());
    assert_eq!(count, 630);
    let mut checked = 0;
    for position in POSITIONS {
        for liveness in LIVENESS {
            if let Some(reason) = resource_cell_exclusion(position, liveness) {
                eprintln!("excluded no-as {position:?}/{liveness:?}: {reason}");
                continue;
            }
            let path = directory
                .path()
                .join(position.label())
                .join(liveness.label())
                .join(Quality::Valid.label());
            let json = carina_cli::commands::validate::validate_diagnostics_with_factories(
                &carina_core::paths::ResolvedPath::new(&path).unwrap(),
                carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
                Default::default(),
            )
            .json;
            // Derived from main's schema-None and undefined-reference exits;
            // every other twin has only fitting literal values and valid names.
            let expected = if matches!(
                liveness,
                Liveness::DestinationUnknown | Liveness::OrdinaryStopped
            ) {
                "error"
            } else {
                "ok"
            };
            assert_eq!(
                json["status"], expected,
                "{position:?}/{liveness:?}: {json}"
            );
            for finding in json
                .get("errors")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                assert!(
                    !finding["code"]
                        .as_str()
                        .is_some_and(|code| code.starts_with("conversion_")),
                    "{json}"
                );
                assert!(
                    !finding["message"]
                        .as_str()
                        .unwrap()
                        .contains("bug in carina"),
                    "{json}"
                );
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 142);
}
