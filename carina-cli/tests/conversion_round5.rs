#[path = "support/conversion_factory.rs"]
mod fixtures;
use carina_core::{
    paths::ResolvedPath, provider::loaded_provider_snapshot_from_factories, schema::SchemaRegistry,
};
use fixtures::*;
use std::sync::Arc;

fn validate(path: &std::path::Path) -> serde_json::Value {
    carina_cli::commands::validate::validate_diagnostics_with_factories(
        &ResolvedPath::new(path).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
        Default::default(),
    )
    .json
}
fn conversions(json: &serde_json::Value) -> usize {
    json["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finding| {
            finding["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("conversion_"))
        })
        .count()
}
fn lsp(source: &str, path: &std::path::Path) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine =
        carina_lsp::diagnostics::DiagnosticEngine::new(Arc::new(loaded), vec!["mock".into()]);
    let doc = carina_lsp::document::Document::new_with_path(
        source.into(),
        path.to_path_buf(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine.analyze(&doc, Some(path))
}
fn write(path: &std::path::Path, source: &str) {
    std::fs::write(path.join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        path.join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .unwrap();
    std::fs::write(path.join("main.crn"), source).unwrap();
}

#[test]
fn round5_unentered_slots_keep_only_mains_ordinary_findings() {
    for attributes in [
        "atr = 'ok' as mock.Name",
        "read_only = 'ok' as mock.Name",
        "config = { unknown = 'ok' as mock.Name }",
        "attr = ['ok' as mock.Name]",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = format!("mock.Sink {{ {attributes} }}\n");
        let twin = source.replace(" as mock.Name", "");
        let mut ordinary = Vec::new();
        let mut lsp_ordinary = Vec::new();
        for source in [&source, &twin] {
            write(directory.path(), source);
            let json = validate(directory.path());
            assert_eq!(json["status"], "error", "{json}");
            assert_eq!(conversions(&json), 0, "{source}: {json}");
            assert!(!json.to_string().contains("bug in carina"), "{json}");
            ordinary.push(
                json["errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|finding| {
                        (
                            finding["message"].clone(),
                            finding["code"].clone(),
                            finding["severity"].clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            let findings = lsp(source, &directory.path().join("main.crn"));
            assert!(findings.iter().all(|finding| !finding.message.contains("bug in carina") && !matches!(&finding.code,Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code.starts_with("conversion_"))), "{source}: {findings:?}");
            lsp_ordinary.push(
                findings
                    .iter()
                    .map(|finding| {
                        (
                            finding.message.clone(),
                            finding.severity,
                            finding.code.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(ordinary[0], ordinary[1], "{attributes}");
        assert_eq!(lsp_ordinary[0], lsp_ordinary[1], "{attributes}");
    }
}

#[test]
fn round5_block_alias_retains_authored_conversion_admission() {
    let directory = tempfile::tempdir().unwrap();
    for (target, count) in [("mock.Name", 0), ("String", 1)] {
        let source = format!("mock.Sink {{ entry {{ value = 'ok' as {target} }} }}\n");
        write(directory.path(), &source);
        let json = validate(directory.path());
        assert_eq!(conversions(&json), count, "{json}");
        assert_eq!(
            json["status"],
            if count == 0 { "ok" } else { "error" },
            "{json}"
        );
        assert!(!json.to_string().contains("bug in carina"), "{json}");
        let diagnostics = lsp(&source, &directory.path().join("main.crn"));
        let converted = diagnostics.iter().filter(|diagnostic| matches!(&diagnostic.code,Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code.starts_with("conversion_"))).count();
        assert_eq!(converted, count, "{diagnostics:?}");
    }
}

#[test]
fn round5_alias_into_non_slot_sinks_is_rejected_by_placement() {
    for (tail, yet) in [
        ("exports { out = a }", true),
        ("provider mock { endpoint = a }", true),
        ("mock.Sink { attr = upper(a) }", false),
        (
            "fn identity(value: String): String { value }\nmock.Sink { attr = identity(a) }",
            false,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = format!("let a = 'x' as mock.Name\n{tail}\n");
        write(directory.path(), &source);
        if tail.starts_with("provider") {
            std::fs::write(directory.path().join("providers.crn"), "").unwrap();
        }
        let json = validate(directory.path());
        assert_eq!(conversions(&json), 1, "{source}: {json}");
        let finding = json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["code"] == "conversion_unsupported_position")
            .unwrap_or_else(|| panic!("{json}"));
        assert_eq!(
            finding["message"],
            if yet {
                "conversion is not supported at this position yet"
            } else {
                "conversion is not supported at this position"
            },
            "{json}"
        );
    }
}

#[test]
fn round5_only_resolver_templates_publish_module_placement_findings() {
    for (called, body, expected) in [
        (false, "mock.Sink { attr = 'ok' as mock.Name }", 0),
        (false, "mock.Sink { attr = 42 as mock.Name }", 0),
        (false, "mock.Sink { attr = upper('ok' as mock.Name) }", 1),
        (true, "mock.Sink { attr = 42 as mock.Name }", 1),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = format!(
            "let child = use {{ source = './child' }}\n{}",
            if called {
                "let net = child { unused = 'ok' }\n"
            } else {
                ""
            }
        );
        write(directory.path(), &source);
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(
            directory.path().join("child/main.crn"),
            format!("arguments {{ unused: String }}\n{body}\n"),
        )
        .unwrap();
        let json = validate(directory.path());
        assert_eq!(conversions(&json), expected, "called={called}: {json}");
        assert!(!json.to_string().contains("bug in carina"), "{json}");
    }
}

#[test]
fn round5_a_failed_module_require_keeps_already_judged_placement_findings() {
    let directory = tempfile::tempdir().unwrap();
    write(
        directory.path(),
        "let child = use { source = './child' }\nlet net = child { unused = 'ok' }\n",
    );
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("child/main.crn"), "arguments { unused: String }\nrequire false, 'ordinary require failure'\nmock.Sink { attr = upper('ok' as mock.Name) }\n").unwrap();
    let json = validate(directory.path());
    assert_eq!(conversions(&json), 1, "{json}");
    assert_eq!(json["errors"].as_array().unwrap().len(), 2, "{json}");
    assert!(json.to_string().contains("ordinary require failure"));
    assert!(!json.to_string().contains("bug in carina"));
}

#[test]
fn round5_directory_require_alias_is_checked_by_placement() {
    let directory = tempfile::tempdir().unwrap();
    write(
        directory.path(),
        "let converted = 'ok' as unloaded.Name\nrequire converted == 'ok', 'guard'\n",
    );
    let json = validate(directory.path());
    assert_eq!(conversions(&json), 1, "{json}");
    assert!(
        json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"].is_null()),
        "main's ordinary require finding remains: {json}"
    );
}
