// Recorded without seam mutations on origin/main ea66ae7a157bd6e28f374f80286a29eb8d77a557.
// Only the temporary directory prefix is normalized; diagnostic order is retained.
use super::*;

fn check(name: &str, source: &str, child: Option<&str>) {
    for mismatched in [false, true] {
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
        let schemas = single_schema_map(
            ResourceSchema::new("Probe").attribute(
                AttributeSchema::optional(
                    "value",
                    if mismatched {
                        AttributeType::int()
                    } else {
                        AttributeType::string()
                    },
                )
                .build(),
            ),
        );
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp_file = if matches!(name, "argument" | "for_argument") {
            "child/main.crn"
        } else {
            "main.crn"
        };
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, lsp_file);
        let actual = serde_json::to_string(&serde_json::json!({ "cli": cli, "lsp": lsp }))
            .unwrap()
            .replace(fixture.path().to_str().unwrap(), "<fixture>");
        let actual: serde_json::Value = serde_json::from_str(&actual).unwrap();
        let name = format!("{name}_{}", if mismatched { "mismatch" } else { "valid" });
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("seam_round3_main.json")).unwrap();
        assert_eq!(actual, expected[&name], "{name}");
    }
}

#[test]
fn round3_slot_pairing_module_argument() {
    check(
        "argument",
        "let child = use { source = './child' }\nlet first = child { input = 'text' }\n",
        Some("arguments { input: String }\ntest.Probe { value = input }\n"),
    );
}

#[test]
fn round3_slot_pairing_function_call() {
    check(
        "function",
        "fn render(value: String): String { value }\ntest.Probe { value = render('text') }\n",
        None,
    );
}

#[test]
fn round3_slot_pairing_for_function_call() {
    check(
        "for_function",
        "fn render(value: String): String { value }\nlet probes = for item in ['one', 'two'] { test.Probe { value = render(item) } }\n",
        None,
    );
}

#[test]
fn round3_slot_pairing_for_module_argument() {
    check(
        "for_argument",
        "let child = use { source = './child' }\nlet first = child { input = 'text' }\n",
        Some(
            "arguments { input: String }\nlet probes = for _ in ['one', 'two'] { test.Probe { value = input } }\n",
        ),
    );
}

#[test]
fn round3_slot_pairing_expanded_module_argument() {
    check(
        "module_argument",
        "let child = use { source = './child' }\nlet first = child { input = 'text' }\nlet second = child { input = 'text' }\n",
        Some("arguments { input: String }\ntest.Probe { value = input }\n"),
    );
}

#[test]
fn round3_slot_pairing_expanded_module_function_and_loop() {
    check(
        "module_function_loop",
        "let child = use { source = './child' }\nlet first = child { input = 'text' }\n",
        Some(
            "arguments { input: String }\nfn render(value: String): String { value }\nlet probes = for _ in ['one', 'two'] { test.Probe { value = render(input) } }\n",
        ),
    );
}
