use super::*;
use crate::parser::ProviderContext;
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

fn schemas() -> crate::schema::SchemaRegistry {
    let mut schemas = crate::schema::SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("first", AttributeType::int()).build())
            .attribute(AttributeSchema::optional("second", AttributeType::int()).build())
            .attribute(
                AttributeSchema::optional("items", AttributeType::list(AttributeType::string()))
                    .with_block_name("item")
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas
}

fn output(
    judged: &super::super::JudgedFile<
        '_,
        crate::parser::ParsedExportParam,
        crate::parser::ParsedDeferredForPhase,
    >,
) -> Vec<String> {
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), judged.schemas(), &config);
    crate::validation::validate_resource_admissions(
        judged,
        &bindings,
        &std::collections::HashSet::from(["mock".into()]),
        &mut Vec::new(),
    )
    .err()
    .map(|error| error.to_string().lines().map(str::to_owned).collect())
    .unwrap_or_default()
}

fn compare(name: &str, actual: Vec<String>) {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("round2_main.json")).unwrap();
    let actual = serde_json::json!(actual);
    assert_eq!(actual, expected[name], "{name}");
}

#[test]
fn round2_missing_report_preserves_main_resource_error() {
    let schemas = schemas();
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_and_resolve("mock.Test { first = 1 second = 2 }").unwrap();
    let mut judged = super::super::JudgedFile::new(parsed, &schemas, &config);
    let mut report = judged.report().clone();
    Arc::make_mut(&mut report.resource_reports).clear();
    super::super::attach_report(&mut judged.file, report);
    compare("missing_report", output(&judged));
}

#[test]
fn round2_unbound_alias_preserves_main_admission() {
    let schemas = schemas();
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_and_resolve("mock.Test { items = ['value'] }").unwrap();
    let mut judged = super::super::JudgedFile::new(parsed, &schemas, &config);
    for resource in &mut judged.file.resources {
        // allow: direct - test a producer's unbound slot alias
        resource
            .authored_slots
            .unbind_alias_for_test("items", "item"); // SEAM_ONLY_FAULT
    }
    compare("unbound_alias", output(&judged));
}

#[test]
fn round2_unknown_schema_preserves_main_diagnostics() {
    let schemas = schemas();
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_and_resolve("mock.Unknown { first = 1 }").unwrap();
    let judged = super::super::JudgedFile::new(parsed, &schemas, &config);
    compare("unknown_schema", output(&judged));
}
