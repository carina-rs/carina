//! The exact quoted-enum and secret-Union directory probes from carina#4014.
mod support;

use carina_core::{
    resource::{ConcreteValue, DeferredValue, Value},
    schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaKind, SchemaRegistry, TypeError,
        TypeIdentity,
    },
};
use support::fixture::{analyze, engine_with_schemas, write_fixture};

#[test]
fn issue_4014_quoted_enum_and_secret_union_report_once() {
    let versioning = AttributeType::enum_(
        carina_core::schema::enum_identity("VersioningStatus", Some("aws.s3.Bucket")),
        Some(vec!["Enabled".into(), "Suspended".into()]),
        vec![
            ("Enabled".into(), "enabled".into()),
            ("Suspended".into(), "suspended".into()),
        ],
        None,
        None,
    );
    let rejected_string = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Rejected"),
        None,
        None,
        carina_core::schema::validator(|_| {
            Err(TypeError::ValidationFailed {
                message: "not accepted".into(),
            })
        }),
        None,
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        ResourceSchema::new("s3.bucket")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("versioning", versioning).build())
            .attribute(
                AttributeSchema::optional(
                    "iv",
                    AttributeType::union(vec![AttributeType::int(), rejected_string]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = engine_with_schemas(schemas.clone());
    let mut failures = Vec::new();
    for (label, assignment, needle) in [
        ("A", "versioning = \"Bogus\"", "expects an enum identifier"),
        ("E", "iv = secret(\"x\")", "failed validation"),
        ("F", "iv = \"x\"", "not accepted"),
    ] {
        let source = format!("aws.s3.bucket {{\n  name = \"b1\"\n  {assignment}\n}}\n");
        let dir = write_fixture(&[
            ("main.crn", &source),
            ("providers.crn", "provider aws {}\n"),
        ]);
        let diagnostics = analyze(&engine, &dir, "main.crn");
        let matching = diagnostics
            .iter()
            .filter(|d| d.message.contains(needle))
            .collect::<Vec<_>>();
        let (start, end) = if label == "A" { (15, 22) } else { (2, 4) };
        if matching.len() != 1
            || matching[0].range.start != tower_lsp::lsp_types::Position::new(2, start)
            || matching[0].range.end != tower_lsp::lsp_types::Position::new(2, end)
        {
            failures.push(format!(
                "{label}: expected one diagnostic at attribute line 2, got {diagnostics:?}"
            ));
        }
        if label == "E" && matching.iter().any(|d| d.message.contains("'x'")) {
            failures.push("E: secret value leaked into diagnostic".into());
        }
        // Compare with the shared resource report consumed by validate. No
        // provider process or cloud operation is needed for these local checks.
        let value: Value = Value::Concrete(ConcreteValue::String(
            if label == "A" { "Bogus" } else { "x" }.into(),
        ));
        let value = if label == "E" {
            Value::Deferred(DeferredValue::Secret(Box::new(value).into()))
        } else {
            value
        };
        let attribute = if label == "A" { "versioning" } else { "iv" };
        let context =
            carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let report = carina_core::test_utils::admit_resource_attribute_report(
            schemas
                .get("aws", "s3.bucket", SchemaKind::Resource)
                .unwrap(),
            &indexmap::IndexMap::from([(
                attribute.into(),
                value
                    .knowledge(&carina_core::resource::knowledge::KnowledgeContext::validation())
                    .unwrap()
                    .into_classification(),
            )]),
            &|name| name == "versioning",
            &context.custom_types,
        );
        let messages = report.errors().map(ToString::to_string).collect::<Vec<_>>();
        if messages.len() != 1
            || matching
                .iter()
                .any(|diagnostic| diagnostic.message != messages[0])
        {
            failures.push(format!(
                "{label}: LSP and validate text differ: LSP={matching:?}, validate={messages:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
