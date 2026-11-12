// Diagnostic goldens were recorded from origin/main ea66ae7a157bd6e28f374f80286a29eb8d77a557
// in an isolated source snapshot and target directory.
use super::*;
use carina_core::schema::{FieldGroup, TypeIdentity};

fn entry() -> AttributeType {
    AttributeType::struct_with_field_groups(
        "Entry",
        vec![
            StructField::required("needed", AttributeType::string()).build(),
            StructField::optional("first", AttributeType::string()).build(),
            StructField::optional("second", AttributeType::string()).build(),
            StructField::optional("count", AttributeType::int()).build(),
            StructField::optional(
                "native",
                AttributeType::refined_string_with_validator(
                    TypeIdentity::bare("Native"),
                    None,
                    None,
                    legacy_validator(|_| Err("native finding".into())),
                    None,
                ),
            )
            .build(),
        ],
        vec![FieldGroup::exactly_one(["first", "second"]).unwrap()],
    )
}

fn schemas() -> SchemaRegistry {
    single_schema_map(
        ResourceSchema::new("Probe")
            .attribute(AttributeSchema::optional("payload", entry()).build())
            .attribute(
                AttributeSchema::optional("mapping", AttributeType::map(AttributeType::int()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("items", AttributeType::list(entry()))
                    .with_block_name("item")
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("unordered", AttributeType::unordered_list(entry()))
                    .build(),
            )
            .attribute(AttributeSchema::optional("sibling", AttributeType::int()).build())
            .attribute(AttributeSchema::optional("line\\nbreak", AttributeType::int()).build())
            .attribute(
                AttributeSchema::optional(
                    "names",
                    AttributeType::map_with_key(
                        AttributeType::enum_(
                            TypeIdentity::new(Some("test"), Vec::<String>::new(), "Key"),
                            Some(vec!["alpha-beta".into()]),
                            vec![("alpha-beta".into(), "alpha_beta".into())],
                            None,
                            None,
                        ),
                        AttributeType::int(),
                    ),
                )
                .build(),
            ),
    )
}

fn check(name: &str, source: &str, child: Option<&str>, extra: Option<&str>) {
    let fixture = write_fixture(&[
        ("providers.crn", "provider test {}\n"),
        ("main.crn", source),
        ("exports.crn", "exports {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ]);
    if let Some(child) = child {
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(fixture.path().join("child/main.crn"), child).unwrap();
    }
    if let Some(extra) = extra {
        std::fs::write(fixture.path().join("values.crn"), extra).unwrap();
    }
    let registry = schemas();
    let mut cli = cli_diagnostics(factories_for(&registry), &fixture);
    // Main emits resource attributes from a HashMap; preserve each message's
    // bytes while comparing that unordered CLI collection as a multiset.
    cli.sort();
    let engine = engine_with_schemas(registry);
    let lsp = if name.starts_with("buffer_context") {
        let path = fixture.path().join("main.crn");
        let doc = Document::new_with_path(
            source.to_owned(),
            std::path::PathBuf::from("test.crn"),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        engine.analyze(&doc, Some(&path))
    } else {
        lsp_diagnostics(&engine, &fixture, "main.crn")
    };
    // TempDir names vary between runs; retain all other bytes and array order.
    let actual = serde_json::to_string(&serde_json::json!({ "cli": cli, "lsp": lsp }))
        .unwrap()
        .replace(fixture.path().to_str().unwrap(), "<fixture>");
    let actual: serde_json::Value = serde_json::from_str(&actual).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("seam_round1_main.json")).unwrap();
    assert_eq!(actual, expected[name], "{name}");
}

#[test]
fn round1_seam_quoted_struct() {
    check(
        "quoted_struct",
        "test.Probe { payload = { 'needed' = 'ok' 'first' = 'a' 'count' = 1 } }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_struct_siblings() {
    check(
        "struct_siblings",
        "test.Probe { payload = { 'unknown' = 1 count = 'bad' native = 'bad' } sibling = 'bad' }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_map_siblings() {
    check(
        "map_siblings",
        "test.Probe { mapping = { 'first' = 'bad' 'second' = 'bad' } sibling = 'bad' }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_list_siblings() {
    check(
        "list_siblings",
        "test.Probe { items = [{ 'unknown' = 1 }, { needed = 'ok' first = 'a' count = 'bad' }] sibling = 'bad' }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_unordered_correspondence() {
    check(
        "unordered",
        "test.Probe { unordered = [{ needed = 'z' first = 'a' }, { needed = 'a' first = 'b' count = 1 }] }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_canonical_map_key() {
    check(
        "canonical_map_key",
        "test.Probe { names = { alpha_beta = 1 } }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_block_alias_keys() {
    check(
        "block_alias",
        "test.Probe { item { needed = 'ok' first = 'a' count = 'bad' } }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_module_argument_if() {
    check(
        "module_if",
        "let child = use { source = './child' }\nlet example = child { flag = true }\n",
        Some(
            "arguments { flag: Bool }\ntest.Probe { payload = if flag { { needed = 'ok' first = 'a' } } else { { needed = 'no' first = 'b' count = 1 } } }\n",
        ),
        None,
    );
}

#[test]
fn round1_seam_module_computed_if() {
    check(
        "module_computed_if",
        "let child = use { source = './child' }\nlet example = child { flag = false }\n",
        Some(
            "arguments { flag: Bool }\nfn choose(value: Bool): Bool { value }\ntest.Probe { payload = if choose(flag) { { needed = 'ok' first = 'a' } } else { { needed = 'no' first = 'b' count = 1 } } }\n",
        ),
        None,
    );
}

#[test]
fn round1_seam_directory_instances() {
    check(
        "directory_instances",
        "for item in values { test.Probe { payload = { needed = item first = 'a' } } }\ntest.Probe { sibling = 'bad' }\n",
        None,
        Some("let values = ['one', 'two']\n"),
    );
}

#[test]
fn round1_seam_quoted_resource_attribute() {
    check(
        "quoted_resource",
        "test.Probe { 'payload' = { needed = 'ok' first = 'a' } }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_quoted_resource_siblings() {
    check(
        "quoted_resource_siblings",
        "test.Probe { 'payload' = { needed = 'ok' first = 'a' } sibling = 'bad' mapping = { key = 'bad' } }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_escaped_map_key() {
    check(
        "escaped_map",
        r#"test.Probe { mapping = { "slash\\key" = 1 } }
"#,
        None,
        None,
    );
}

#[test]
fn round1_seam_escaped_unknown_struct_field() {
    check(
        "escaped_unknown",
        r#"test.Probe { payload = { "unknown\\key" = 1 count = 'bad' native = 'bad' } sibling = 'bad' }
"#,
        None,
        None,
    );
}

#[test]
fn round1_seam_module_access_if() {
    check(
        "module_access_if",
        "let child = use { source = './child' }\nlet example = child { settings = { enabled = true } }\n",
        Some(
            "arguments { settings: map(Bool) }\ntest.Probe { payload = if settings['enabled'] { { needed = 'ok' first = 'a' } } else { { needed = 'no' first = 'b' } } }\n",
        ),
        None,
    );
}

#[test]
fn round1_seam_escaped_resource_attribute() {
    check(
        "escaped_resource",
        r#"test.Probe { "line\nbreak" = 1 }
"#,
        None,
        None,
    );
}

#[test]
fn round1_seam_escaped_resource_siblings() {
    check(
        "escaped_resource_siblings",
        r#"test.Probe { "line\nbreak" = 1 sibling = 'bad' mapping = { key = 'bad' } }
"#,
        None,
        None,
    );
}

#[test]
fn round1_seam_buffer_context_without_file_identity() {
    check(
        "buffer_context",
        "test.Probe { payload = { needed = 'ok' first = 'a' } }\n",
        None,
        None,
    );
}

#[test]
fn round1_seam_buffer_context_keeps_later_schema_diagnostics() {
    check(
        "buffer_context_siblings",
        "test.Probe { sibling = 'bad' }\n",
        None,
        None,
    );
}
