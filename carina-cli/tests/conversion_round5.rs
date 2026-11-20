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
    let engine = carina_lsp::test_support::Diagnostics::new(
        path.parent().unwrap(),
        Arc::new(loaded),
        vec!["mock".into()],
    );
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
fn r11_resolver_templates_publish_placement_and_judge_static_findings() {
    for (called, body, expected) in [
        (false, "mock.Sink { attr = 'ok' as mock.Name }", 0),
        (false, "mock.Sink { attr = 42 as mock.Name }", 1),
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
    std::fs::write(directory.path().join("child/main.crn"), "arguments { unused: String \nrequire false, 'ordinary require failure' }\nmock.Sink { attr = upper('ok' as mock.Name) }\n").unwrap();
    let json = validate(directory.path());
    assert_eq!(conversions(&json), 1, "{json}");
    assert_eq!(json["errors"].as_array().unwrap().len(), 2, "{json}");
    assert!(json.to_string().contains("ordinary require failure"));
    assert!(!json.to_string().contains("bug in carina"));
}

#[test]
fn r5b_cross_file_aliases_report_at_the_producer_in_both_file_orders() {
    for (producer_file, consumer_file) in [("a.crn", "b.crn"), ("b.crn", "a.crn")] {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "");
        let producer = "let converted = 1 as mock.Name\n";
        let consumer = "mock.Sink { attr = converted }\n";
        let producer_path = directory.path().join(producer_file);
        let consumer_path = directory.path().join(consumer_file);
        std::fs::write(&producer_path, producer).unwrap();
        std::fs::write(&consumer_path, consumer).unwrap();
        let json = validate(directory.path());
        let errors = json["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{json}");
        assert_eq!(errors[0]["code"], "conversion_scalar_kind", "{json}");
        assert!(errors[0].to_string().contains(producer_file), "{json}");
        let at_producer = lsp(producer, &producer_path);
        let at_consumer = lsp(consumer, &consumer_path);
        let errors = at_producer
            .iter()
            .chain(&at_consumer)
            .filter(|diagnostic| {
                diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            errors.len(),
            1,
            "producer={at_producer:?}, consumer={at_consumer:?}"
        );
        assert_eq!(
            errors[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "conversion_scalar_kind".into()
            ))
        );
        assert_eq!(
            errors[0].range.start,
            tower_lsp::lsp_types::Position::new(0, 16)
        );
        // LSP diagnostics belong to the open document. A plain use gets no
        // fabricated squiggle for the rejection owned by the producer file.
        assert!(at_consumer.iter().all(|finding| !matches!(&finding.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code.starts_with("conversion_"))));
    }
}

#[test]
fn r5b_non_crossed_edges_have_producer_verdicts_and_plain_consumers() {
    // Mutation audit: the report records which edges have actual child evidence
    // to follow. Type-only parameter/state/output edges cannot be made to
    // propagate a Convert by replacing a cut with Follow over zero children.
    for (producer, consumer, module, expected_code, yet) in [
        (
            "let p = mock.Source { output = 1 as mock.Name }",
            "mock.Sink { attr = p.output }",
            "",
            "conversion_scalar_kind",
            false,
        ),
        (
            "let p = mock.Source { output = 'ok' as mock.Name }",
            "mock.Sink { attr = p.output }",
            "",
            "",
            false,
        ),
        (
            "let child = use { source = './child' }",
            "let net = child {}\nmock.Sink { plain = net.out }",
            "attributes { out: String = 'x' as mock.Name }",
            "conversion_unsupported_position",
            true,
        ),
        (
            "fn f(a: Int): Int { 1 as mock.Name }",
            "let result = f(1)",
            "",
            "conversion_unsupported_position",
            true,
        ),
        (
            "fn f(a: Int = 1 as mock.Name): Int { a }",
            "let result = f()",
            "",
            "conversion_unsupported_position",
            true,
        ),
        (
            "fn f(a: Int): Int { a }\nlet converted = 1 as mock.Name",
            "let result = f(converted)",
            "",
            "conversion_unsupported_position",
            false,
        ),
        (
            "let child = use { source = './child' }\nlet converted = 'x' as mock.Name",
            "let net = child { input = converted }\nmock.Sink { plain = net.out }",
            "arguments { input: String }\nattributes { out: String = input }",
            "conversion_unsupported_position",
            true,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "");
        std::fs::write(directory.path().join("a.crn"), producer).unwrap();
        std::fs::write(directory.path().join("b.crn"), consumer).unwrap();
        if !module.is_empty() {
            std::fs::create_dir(directory.path().join("child")).unwrap();
            std::fs::write(directory.path().join("child/main.crn"), module).unwrap();
        }
        let json = validate(directory.path());
        let errors = json["errors"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let converted_errors = errors
            .iter()
            .filter(|error| {
                error["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("conversion_"))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            converted_errors.len(),
            usize::from(!expected_code.is_empty()),
            "{producer}\n{consumer}\n{module}: {json}"
        );
        if producer.starts_with("let p") && !expected_code.is_empty() {
            // The plain consumer is unclaimed: 1b-rules must still check its
            // lowered Int value exactly as on main. Suppressing that finding
            // would contradict the single claim/reference composition rule.
            let consumer_errors = |json: &serde_json::Value| {
                json["errors"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter(|error| error["code"].is_null() && error.to_string().contains("b.crn"))
                    .map(|error| error["message"].clone())
                    .collect::<Vec<_>>()
            };
            std::fs::write(
                directory.path().join("a.crn"),
                producer.replace(" as mock.Name", ""),
            )
            .unwrap();
            let twin = validate(directory.path());
            assert_eq!(consumer_errors(&json), consumer_errors(&twin));
            assert_eq!(consumer_errors(&json).len(), 1);
        } else {
            assert_eq!(errors.len(), converted_errors.len(), "{json}");
        }
        if let Some(error) = converted_errors.first() {
            assert_eq!(error["code"], expected_code, "{json}");
            let owner = if module.contains(" as ") {
                "child/main.crn"
            } else {
                "a.crn"
            };
            assert!(error.to_string().contains(owner), "{json}");
            if expected_code == "conversion_unsupported_position" {
                assert_eq!(
                    error["message"].as_str().unwrap().ends_with("yet"),
                    yet,
                    "{json}"
                );
            }
        }
        if expected_code.is_empty() {
            std::fs::write(
                directory.path().join("a.crn"),
                producer.replace(" as mock.Name", ""),
            )
            .unwrap();
            let twin = validate(directory.path());
            assert_eq!(json["errors"], twin["errors"]);
            assert_eq!(json["status"], twin["status"]);
        }
    }
}

#[test]
fn r5b_valid_conversion_in_a_block_alias_uses_authored_admission() {
    let directory = tempfile::tempdir().unwrap();
    for source in [
        "mock.Sink { entry { value = 'ok' as mock.Name } }",
        "mock.Sink { entry { value = 'ok' } }",
    ] {
        write(directory.path(), source);
        let json = validate(directory.path());
        assert_eq!(json["status"], "ok", "{json}");
        assert_eq!(
            json["errors"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .len(),
            0,
            "{json}"
        );
        assert!(
            lsp(source, &directory.path().join("main.crn"))
                .iter()
                .all(|finding| finding.severity
                    != Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR))
        );
    }
}

#[test]
fn r5b_upstream_cut_keeps_the_producer_rejection_out_of_the_plain_consumer() {
    use carina_core::{
        parser::ExpandedBackendConfig,
        resource::{ConcreteValue, Value},
    };
    use carina_state::{BackendLock, StateFile};
    let directory = tempfile::tempdir().unwrap();
    let upstream = directory.path().join("upstream");
    let downstream = directory.path().join("downstream");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::create_dir(&downstream).unwrap();
    write(&upstream, "");
    std::fs::write(upstream.join("a.crn"), "let x = 'ok' as mock.Name").unwrap();
    std::fs::write(upstream.join("b.crn"), "exports { v: String = x }").unwrap();
    let mut state = StateFile::new();
    state.insert_export_json_for_test("v", serde_json::json!("ok"));
    std::fs::write(
        upstream.join("carina.state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let backend = ExpandedBackendConfig {
        backend_type: "local".into(),
        attributes: std::collections::HashMap::from([(
            "path".to_string(),
            Value::Concrete(ConcreteValue::String("carina.state.json".into())),
        )])
        .into(),
        attribute_spans: Default::default(),
    };
    BackendLock::for_config(Some(&backend))
        .unwrap()
        .save(&ResolvedPath::new(&upstream).unwrap())
        .unwrap();
    let producer = validate(&upstream);
    assert_eq!(conversions(&producer), 1, "{producer}");
    let error = &producer["errors"][0];
    assert_eq!(error["code"], "conversion_unsupported_position");
    assert!(
        error["message"].as_str().unwrap().ends_with("yet"),
        "{producer}"
    );
    assert!(error.to_string().contains("a.crn"), "{producer}");
    write(
        &downstream,
        "let up = upstream_state { source = '../upstream' }",
    );
    std::fs::write(
        downstream.join("consumer.crn"),
        "mock.Sink { plain = up.v }",
    )
    .unwrap();
    let consumer = validate(&downstream);
    assert_eq!(consumer["status"], "ok", "{consumer}");
    assert_eq!(conversions(&consumer), 0, "{consumer}");
    std::fs::write(upstream.join("a.crn"), "let x = 'ok'").unwrap();
    let twin = validate(&downstream);
    assert_eq!(consumer["errors"], twin["errors"]);
    assert_eq!(consumer["status"], twin["status"]);
}
