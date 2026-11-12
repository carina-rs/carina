//! Whole-file protection for unparseable source; parsed files retain key provenance.
use carina_core::{parser::ProviderContext, provider::ProviderFactory, schema::SchemaRegistry};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use std::{path::Path, process::Command, sync::Arc};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

fn validate(path: &Path, json: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .arg("validate")
        .arg(path)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    if json {
        command.arg("--json");
    }
    command.output().unwrap()
}
fn diagnostics(source: &str, path: &Path) -> Vec<Diagnostic> {
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let snapshot = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(snapshot), vec!["mock".into()]);
    let document = Document::new_with_path(
        source.to_owned(),
        path.to_owned(),
        Arc::new(ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine.analyze(&document, Some(path))
}

#[test]
fn round13_unparseable_secret_files_expose_only_kind_and_position() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    for source in [
        "let CANARY_BadName = 1\nlet hidden = secret('CANARY')\nlet y = ???\n",
        "let hidden = 'CANARY' |> secret()\nlet y = ???\n",
        "let hidden = secret /* c */ ('CANARY')\nlet y = ???\n",
        "let hidden = <<MARKER\nCANARY\nMARKER\n |> secret()\nlet y = ???\n",
        "let hidden = secret(<<CANARY\npayload\n",
        "requiresecret('CANARY'), 'no' ???\n",
        "ifsecret('CANARY') { 1 } ???\n",
        "let hidden = insecret('CANARY') ???\n",
        "let before = 'CANARY'\nlet hidden = secret('x')\nlet CANARY_BadName = 1\nlet y = ???\n",
        "requiresecret(<<CANARY\n(((((((((\n",
        "let hidden = secret 'CANARY'",
        "let hidden = secret\n'CANARY'",
        "let hidden = secret,'CANARY'",
        "requiresecret 'CANARY'",
        "let hidden = secret('CANARY',)",
        "let hidden = secret('CANARY",
        "/* outer /* inner */ secret('CANARY')",
        "let text = <<EOF\n${secret('\nlet CANARY_BadName = 1\n')}\n",
    ] {
        std::fs::write(&path, source).unwrap();
        let human = validate(fixture.path(), false);
        let json = validate(fixture.path(), true);
        assert_eq!(human.status.code(), Some(1));
        assert_eq!(json.status.code(), Some(1));
        assert!(json.stderr.is_empty());
        let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let findings = diagnostics(source, &path);
        let error = carina_core::parser::parse_file(
            source,
            &path,
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        )
        .unwrap_err();
        let load_failure = carina_core::config_loader::load_configuration_report(
            &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            &SchemaRegistry::new(),
        )
        .err()
        .expect("broken directory is a typed loading failure");
        let failure = error
            .protected_failure()
            .expect("only kind and position survive");
        assert_eq!(value["errors"].as_array().unwrap().len(), 1);
        assert_eq!(value["errors"][0]["message"], failure.kind().to_string());
        assert_eq!(findings[0].message, failure.to_string());
        assert!(String::from_utf8_lossy(&human.stderr).contains(&failure.to_string()));
        let rendered = format!(
            "{}{}{} {findings:?} {error:?} {load_failure:?}",
            String::from_utf8_lossy(&human.stdout),
            String::from_utf8_lossy(&human.stderr),
            String::from_utf8_lossy(&json.stdout)
        );
        assert!(!rendered.contains("CANARY"), "{rendered}");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(value.get("warnings").is_none(), "{value}");
        assert!(
            value["errors"][0]["location"]["range"].is_object(),
            "{value}"
        );
        assert!(!rendered.contains("-->"), "{rendered}");
    }
}

#[test]
fn round13_directory_protection_is_per_file() {
    let fixture = tempfile::tempdir().unwrap();
    let private = "let CANARY_BadName = 1\nlet hidden = secret('CANARY')\nlet y = ???\n";
    let public = "let BadPublic = 1\nlet x = ???\n";
    let parsed = "let BadParsed = 'secret'\n";
    for (name, source) in [("a.crn", public), ("b.crn", private), ("c.crn", parsed)] {
        std::fs::write(fixture.path().join(name), source).unwrap();
    }
    let human = validate(fixture.path(), false);
    let json = validate(fixture.path(), true);
    let text = format!(
        "{}{}{}",
        String::from_utf8_lossy(&human.stdout),
        String::from_utf8_lossy(&human.stderr),
        String::from_utf8_lossy(&json.stdout)
    );
    assert!(!text.contains("CANARY"), "{text}");
    assert!(String::from_utf8_lossy(&human.stderr).contains("let x = ???"));
    let private_findings = diagnostics(private, &fixture.path().join("b.crn"));
    assert_eq!(private_findings.len(), 1, "{private_findings:?}");
    assert!(!format!("{private_findings:?}").contains("CANARY"));
    let public_findings = diagnostics(public, &fixture.path().join("a.crn"));
    assert_eq!(
        public_findings[0].message,
        " --> 2:9\n  |\n2 | let x = ???\n  |         ^---\n  |\n  = expected use_expr or primary"
    );
    assert!(
        public_findings
            .iter()
            .any(|d| d.message.starts_with("Binding 'BadPublic'")),
        "{public_findings:?}"
    );
    assert!(
        diagnostics(parsed, &fixture.path().join("c.crn"))
            .iter()
            .any(|d| d.message.starts_with("Binding 'BadParsed'"))
    );
}

#[test]
fn round13_public_tag_warning_does_not_reattach_to_a_secret_key() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "provider mock {}\nmock.test.resource { name = 'a'\n tags = secret({ BadKey = 'private' })\n}\nmock.test.resource { name = 'b'\n tags = {\n  BadKey = 'public'\n  one = '1'\n  two = '2'\n }\n}\n";
    std::fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .arg("lint")
        .arg(fixture.path())
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .unwrap();
    let human = String::from_utf8(output.stderr).unwrap();
    let message = "Tag key 'BadKey' doesn't match the dominant style (snake_case). Use consistent casing for tag keys.";
    assert_eq!(
        human
            .lines()
            .filter(|line| line.starts_with("warning: "))
            .collect::<Vec<_>>(),
        [format!("warning: {}:7  {message}", path.display())]
    );
    let all = diagnostics(source, &path);
    let warnings = all
        .iter()
        .filter(|d| d.message.starts_with("Tag key '"))
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "{all:?}");
    assert_eq!(warnings[0].message, message);
    assert_eq!(warnings[0].range.start.line, 6);
}

#[test]
fn round13_policy_example_output() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("main.crn");
    let source = "let hidden = secret('x')\nlet BadName = 1\nlet y = ???\n";
    std::fs::write(&path, source).unwrap();
    let human = validate(fixture.path(), false);
    let json = validate(fixture.path(), true);
    let findings = diagnostics(source, &path);
    println!(
        "POLICY_EXAMPLE={}",
        serde_json::json!({"human_stdout":String::from_utf8_lossy(&human.stdout),"human_stderr":String::from_utf8_lossy(&human.stderr),"json_stdout":String::from_utf8_lossy(&json.stdout).replace(fixture.path().to_str().unwrap(), "<fixture>"),"json_stderr":String::from_utf8_lossy(&json.stderr),"lsp":findings})
    );
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(!String::from_utf8_lossy(&human.stderr).contains("let y"));
}
