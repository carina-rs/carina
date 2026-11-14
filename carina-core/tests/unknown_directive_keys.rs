use std::path::Path;

use carina_core::parser::{ProviderContext, parse_file, parse_syntax_inventory};
use carina_core::validation::unknown_directive_keys;

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
        let findings = unknown_directive_keys(parsed.parser_binding_metadata.syntax());
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
  config { directives { nested_attribute = true } }
  directives { unsupported_key = { nested_attribute = true } another_key = absent }
}
"#;
    let syntax = parse_syntax_inventory(source, Path::new("resources.crn")).unwrap();
    let findings = unknown_directive_keys(&syntax);
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
    assert!(unknown_directive_keys(parsed.parser_binding_metadata.syntax()).is_empty());
    let child = &parsed.resources[1];
    assert!(!child.dependencies.depends_on.is_empty());
    assert!(!child.attributes.contains_key("depends_on"));
    assert_eq!(child.directives.ignore_changes.len(), 1);
    assert_eq!(
        child.directives.provider_instance.as_deref(),
        Some("alternate")
    );
}
