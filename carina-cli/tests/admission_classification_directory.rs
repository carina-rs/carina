#[path = "../../carina-lsp/tests/support/fixture.rs"]
mod fixture;

use std::path::Path;
use std::process::Command;

#[test]
fn multifile_admission_classification_preserves_validate_and_lsp_diagnostics() {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/admission_classification");
    let mut records = Vec::new();
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&root)
            .args(["validate", "."])
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE");
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        records.push(serde_json::json!({
            "mode": if json { "json" } else { "text" },
            "status": output.status.code(),
            "stdout": String::from_utf8(output.stdout).unwrap().replace(root.to_str().unwrap(), "$PROJECT"),
            "stderr": String::from_utf8(output.stderr).unwrap().replace(root.to_str().unwrap(), "$PROJECT"),
        }));
    }
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    for schema in carina_provider_mock::builtin_schemas() {
        schemas.insert("mock", schema);
    }
    let engine = fixture::engine_with_schemas(schemas);
    for name in [
        "main.crn",
        "exports.crn",
        "providers.crn",
        "backend.crn",
        "module/main.crn",
        "module/attributes.crn",
    ] {
        let path = root.join(name);
        let document = carina_lsp::document::Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            std::sync::Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let diagnostics = engine.analyze(&document, Some(&path));
        let diagnostics = serde_json::to_string(&diagnostics)
            .unwrap()
            .replace(root.to_str().unwrap(), "$PROJECT");
        records.push(serde_json::json!({"document": name, "diagnostics": serde_json::from_str::<serde_json::Value>(&diagnostics).unwrap()}));
    }
    insta::assert_snapshot!(serde_json::to_string_pretty(&records).unwrap());
}
