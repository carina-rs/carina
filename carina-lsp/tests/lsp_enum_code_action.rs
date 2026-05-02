//! Integration coverage for #2309: enum-mismatch diagnostics carry a
//! structured payload on `Diagnostic.data` and the
//! `textDocument/codeAction` handler turns that payload into one
//! quick-fix per candidate.
//!
//! Each scenario builds a directory-shaped fixture (`main.crn` plus a
//! sibling file) under `tempfile::tempdir()`, runs the
//! `DiagnosticEngine` to produce diagnostics, then exercises the
//! `code_actions_for_diagnostic` consumer directly. Going through the
//! engine + the public consumer (rather than the tower-lsp socket)
//! keeps the test focused on the contract under #2309: structured
//! payload in, `WorkspaceEdit` out. Editor-side wiring is out of
//! scope per the issue.

mod support;

use std::collections::HashMap;

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use carina_lsp::code_action::{
    EnumDiagnosticData, EnumDiagnosticKind, code_actions_for_diagnostic,
};
use support::fixture::{analyze, engine_with_schemas, write_fixture};
use tower_lsp::lsp_types::{Diagnostic, Url};

fn versioning_schema() -> HashMap<String, ResourceSchema> {
    fn lower(v: &str) -> String {
        v.to_ascii_lowercase()
    }
    let versioning = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: Some("aws.s3.Bucket".to_string()),
        to_dsl: Some(lower),
    };
    let mut schemas = HashMap::new();
    schemas.insert(
        "aws.s3.bucket".to_string(),
        ResourceSchema::new("aws.s3.bucket")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("versioning", versioning)),
    );
    schemas
}

fn first_enum_diagnostic(diags: &[Diagnostic]) -> &Diagnostic {
    diags
        .iter()
        .find(|d| EnumDiagnosticData::from_diagnostic(d).is_some())
        .unwrap_or_else(|| {
            panic!(
                "expected at least one diagnostic with EnumDiagnosticData, got: {:#?}",
                diags
            )
        })
}

fn dummy_uri() -> Url {
    Url::parse("file:///tmp/main.crn").unwrap()
}

// ---------------------------------------------------------------------------
// Scenario 1: bare-identifier mismatch on a namespaced StringEnum
// ---------------------------------------------------------------------------

#[test]
fn bare_identifier_invalid_emits_payload_and_quick_fix() {
    let engine = engine_with_schemas(versioning_schema());
    // Multi-file fixture per the directory-scoped invariant: providers.crn
    // sits next to main.crn so the engine sees the directory shape.
    let fixture = write_fixture(&[
        (
            "main.crn",
            "aws.s3.bucket {\n  name = \"b1\"\n  versioning = aws.s3.Bucket.VersioningStatus.Bogus\n}\n",
        ),
        ("providers.crn", "provider aws {}\n"),
    ]);
    let diags = analyze(&engine, &fixture, "main.crn");
    let diag = first_enum_diagnostic(&diags);

    let payload = EnumDiagnosticData::from_diagnostic(diag).expect("payload present");
    assert_eq!(payload.kind, EnumDiagnosticKind::BareInvalid);
    // Canonical entries first; aliases (`enabled`, `suspended` from the
    // `to_dsl` lowercase mapping) follow.
    let canonicals: Vec<_> = payload
        .expected
        .iter()
        .filter(|e| !e.is_alias)
        .map(|e| e.value.clone())
        .collect();
    assert_eq!(
        canonicals,
        vec!["Enabled".to_string(), "Suspended".to_string()]
    );

    let actions = code_actions_for_diagnostic(&dummy_uri(), diag);
    let titles: Vec<_> = actions.iter().map(|a| a.title.clone()).collect();
    assert_eq!(
        titles,
        vec![
            "Replace with `aws.s3.Bucket.VersioningStatus.Enabled`".to_string(),
            "Replace with `aws.s3.Bucket.VersioningStatus.Suspended`".to_string(),
        ],
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: quoted string literal on a StringEnum (StringLiteral kind)
// ---------------------------------------------------------------------------

#[test]
fn string_literal_emits_string_literal_kind_and_replaces_quotes() {
    let engine = engine_with_schemas(versioning_schema());
    let fixture = write_fixture(&[
        (
            "main.crn",
            "aws.s3.bucket {\n  name = \"b1\"\n  versioning = \"Bogus\"\n}\n",
        ),
        ("providers.crn", "provider aws {}\n"),
    ]);
    let diags = analyze(&engine, &fixture, "main.crn");
    let diag = first_enum_diagnostic(&diags);
    let payload = EnumDiagnosticData::from_diagnostic(diag).expect("payload present");
    assert_eq!(payload.kind, EnumDiagnosticKind::StringLiteral);

    // The diagnostic range must cover both quote characters so the
    // applied edit drops them when writing the bare identifier.
    let text = std::fs::read_to_string(fixture.path().join("main.crn")).expect("read fixture file");
    let line = text.lines().nth(2).expect("third line");
    let open = line.find('"').expect("opening quote") as u32;
    let close = (line.rfind('"').expect("closing quote") + 1) as u32;
    assert_eq!(diag.range.start.character, open);
    assert_eq!(diag.range.end.character, close);

    let actions = code_actions_for_diagnostic(&dummy_uri(), diag);
    assert!(
        !actions.is_empty(),
        "code action should be offered for string-literal enum diagnostic"
    );
    // The first action's edit replaces `"Bogus"` (incl. quotes) with the
    // bare identifier — applying it must produce a buffer that
    // re-validates.
    let edit = actions[0].edit.as_ref().unwrap();
    let edits = edit.changes.as_ref().unwrap().get(&dummy_uri()).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].range, diag.range);
    assert_eq!(
        edits[0].new_text, "aws.s3.Bucket.VersioningStatus.Enabled",
        "first canonical candidate replaces the quoted literal"
    );

    // Sanity: applying that edit to the source line should yield a
    // line that is now valid input syntactically — i.e. the literal
    // form was replaced and there are no orphan quotes.
    let mut applied = String::new();
    applied.push_str(&line[..open as usize]);
    applied.push_str(&edits[0].new_text);
    applied.push_str(&line[close as usize..]);
    assert!(
        !applied.contains('"'),
        "applied edit must drop both quotes, got: {applied}"
    );
    assert!(
        applied.contains("aws.s3.Bucket.VersioningStatus.Enabled"),
        "applied edit must contain the canonical identifier, got: {applied}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: non-namespaced StringEnum
// ---------------------------------------------------------------------------

fn bare_mode_schema() -> HashMap<String, ResourceSchema> {
    let mode = AttributeType::StringEnum {
        name: "Mode".to_string(),
        values: vec!["fast".to_string(), "slow".to_string()],
        namespace: None,
        to_dsl: None,
    };
    let mut schemas = HashMap::new();
    schemas.insert(
        "test.r.mode_holder".to_string(),
        ResourceSchema::new("test.r.mode_holder")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("mode", mode)),
    );
    schemas
}

#[test]
fn non_namespaced_enum_quick_fix_uses_bare_value() {
    let engine = engine_with_schemas(bare_mode_schema());
    let fixture = write_fixture(&[
        (
            "main.crn",
            "test.r.mode_holder {\n  name = \"a\"\n  mode = bogus\n}\n",
        ),
        ("providers.crn", "provider test {}\n"),
    ]);
    let diags = analyze(&engine, &fixture, "main.crn");
    let diag = first_enum_diagnostic(&diags);
    let actions = code_actions_for_diagnostic(&dummy_uri(), diag);
    let titles: Vec<_> = actions.iter().map(|a| a.title.clone()).collect();
    assert_eq!(
        titles,
        vec![
            "Replace with `fast`".to_string(),
            "Replace with `slow`".to_string()
        ],
        "non-namespaced enums replace with bare values, no provider prefix"
    );
}
