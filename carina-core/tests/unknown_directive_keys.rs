use std::path::Path;

use carina_core::parser::{ProviderContext, parse_file, parse_syntax_inventory};
use carina_core::resource::{ConcreteValue, Value};
use carina_core::validation::resource_directive_findings;

#[test]
fn non_assignment_directive_entries_are_all_located_without_evaluation() {
    let source = "mock.test.resource {\n  directives {\n    prevent_destroy {}\n    unsupported {}\n    let x = absent\n    force_delte = true\n  }\n}\n";
    let parsed = parse_file(
        source,
        Path::new("resources.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .expect("invalid entries are collected without evaluating their contents");
    let findings = resource_directive_findings(parsed.parser_binding_metadata.syntax());
    assert_eq!(findings.len(), 4, "{findings:?}");
    for (finding, (line, width)) in findings.iter().zip([(3, 15), (4, 11), (5, 14)]) {
        assert_eq!(
            finding.detail().to_string(),
            "Directive entries must be 'key = value' assignments"
        );
        assert_eq!(finding.location().file().path(), Path::new("resources.crn"));
        let span = finding.location().span();
        assert_eq!((span.start_line(), span.start_column()), (line, 5));
        assert_eq!((span.end_line(), span.end_column()), (line, 5 + width));
    }
    assert!(
        findings[3]
            .detail()
            .to_string()
            .contains("Unknown directive 'force_delte'")
    );
}

#[test]
fn invalid_directive_entries_in_unevaluated_resources_are_collected() {
    let source = "let empty = for n in [] {\n  mock.test.resource { directives { let x = absent } }\n}\nlet hidden = if false {\n  mock.test.resource { directives { prevent_destroy {} } }\n}\n";
    let syntax = parse_syntax_inventory(source, Path::new("resources.crn")).unwrap();
    let findings = resource_directive_findings(&syntax);
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert_eq!(findings[0].location().span().start_line(), 2);
    assert_eq!(findings[1].location().span().start_line(), 5);
}

#[test]
fn duplicate_directives_preserve_keys_in_both_blocks_and_declaration_inventory() {
    let source = "let repeated = 1\nlet repeated = 2\nmock.test.resource {\n  directives { force_delte = true }\n  directives { provder = absent }\n}\n";
    let syntax = parse_syntax_inventory(source, Path::new("resources.crn"))
        .expect("duplicate directives must not invalidate the inventory");
    let findings = resource_directive_findings(&syntax);
    assert_eq!(findings.len(), 3, "{findings:?}");
    assert!(findings.iter().any(|finding| {
        finding
            .detail()
            .to_string()
            .contains("Unknown directive 'force_delte'")
    }));
    assert!(findings.iter().any(|finding| {
        finding
            .detail()
            .to_string()
            .contains("Unknown directive 'provder'")
    }));
    let duplicate = findings
        .iter()
        .find(|finding| {
            finding
                .detail()
                .to_string()
                .starts_with("Duplicate attribute 'directives'")
        })
        .unwrap();
    assert_eq!(
        duplicate.detail().to_string(),
        "Duplicate attribute 'directives' (first defined on line 4)"
    );
    let span = duplicate.location().span();
    assert_eq!(
        (
            span.start_line(),
            span.start_column(),
            span.end_line(),
            span.end_column()
        ),
        (5, 3, 5, 13)
    );
    assert_eq!(
        carina_core::config_loader::find_duplicate_declarations_in_source(
            source,
            Path::new("resources.crn")
        )
        .len(),
        1
    );
}

#[test]
fn unknown_directive_keys_use_shared_suggestions_at_each_authored_key() {
    for (key, suggestion) in [
        ("force_delte", Some("force_delete")),
        ("create_before_destory", Some("create_before_destroy")),
        ("prevent_destory", Some("prevent_destroy")),
        ("ignore_change", Some("ignore_changes")),
        ("depend_on", Some("depends_on")),
        ("provder", Some("provider")),
        ("unsupported_key", None),
    ] {
        let source =
            format!("mock.test.resource {{\n  directives {{\n    {key} = true\n  }}\n}}\n");
        let parsed = parse_file(
            &source,
            Path::new("resources.crn"),
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        )
        .expect("unknown keys must not abort parsing");
        let findings = resource_directive_findings(parsed.parser_binding_metadata.syntax());
        assert_eq!(findings.len(), 1, "{key}: {findings:?}");
        let expected = match suggestion {
            Some(name) => format!("Unknown directive '{key}', did you mean '{name}'?"),
            None => format!("Unknown directive '{key}'"),
        };
        assert_eq!(findings[0].detail().to_string(), expected);
        let location = findings[0].location();
        assert_eq!(location.file().path(), Path::new("resources.crn"));
        assert_eq!(location.span().start_line(), 3);
        assert_eq!(location.span().end_line(), 3);
        assert_eq!(location.span().start_column(), 5);
        assert_eq!(location.span().end_column(), 5 + key.len());
    }
}

#[test]
fn directive_inventory_includes_unevaluated_resources_but_not_ordinary_nested_keys() {
    let source = r#"let empty = for n in [] {
  mock.test.resource { directives { prevent_destory = true } }
}
let hidden = if false { mock.test.resource { directives { provder = absent } } }
let read_back = read mock.test.resource { directives { ignore_change = [] } }
mock.test.resource {
  config {
    directives { nested_attribute = true }
    deeper { directives { prevent_destory = true provider = "ordinary" } }
  }
  directives { unsupported_key = { nested_attribute = true } another_key = absent }
}
"#;
    let syntax = parse_syntax_inventory(source, Path::new("resources.crn")).unwrap();
    let findings = resource_directive_findings(&syntax);
    let messages: Vec<_> = findings
        .iter()
        .map(|finding| finding.detail().to_string())
        .collect();
    assert_eq!(
        messages,
        [
            "Unknown directive 'prevent_destory', did you mean 'prevent_destroy'?",
            "Unknown directive 'provder', did you mean 'provider'?",
            "Unknown directive 'ignore_change', did you mean 'ignore_changes'?",
            "Unknown directive 'unsupported_key'",
            "Unknown directive 'another_key'",
        ]
    );
    let parsed = parse_file(
        source,
        Path::new("resources.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .expect("nested directives use ordinary attribute semantics");
    let Value::Concrete(ConcreteValue::List(config)) = &parsed.resources[0].attributes["config"]
    else {
        panic!("config block was not retained");
    };
    let Value::Concrete(ConcreteValue::Map(config)) = &config[0] else {
        panic!("config attributes were not retained");
    };
    assert!(config.contains_key("directives"));
    let Value::Concrete(ConcreteValue::List(deeper)) = &config["deeper"] else {
        panic!("deeper block was not retained");
    };
    let Value::Concrete(ConcreteValue::Map(deeper)) = &deeper[0] else {
        panic!("deeper attributes were not retained");
    };
    assert!(deeper.contains_key("directives"));
}

#[test]
fn recognized_directive_keys_still_parse_with_typed_depends_on() {
    let source = r#"provider mock {}
let parent = mock.test.resource { name = "parent" }
mock.test.resource {
  name = "child"
  directives {
    force_delete = false
    create_before_destroy = true
    prevent_destroy = true
    ignore_changes = [name]
    depends_on = [parent]
    provider = alternate
  }
}
"#;
    let parsed = parse_file(
        source,
        Path::new("resources.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert!(resource_directive_findings(parsed.parser_binding_metadata.syntax()).is_empty());
    let child = &parsed.resources[1];
    assert!(!child.dependencies.depends_on.is_empty());
    assert!(!child.attributes.contains_key("depends_on"));
    assert_eq!(child.directives.ignore_changes.len(), 1);
    assert_eq!(
        child.directives.provider_instance.as_deref(),
        Some("alternate")
    );
}
