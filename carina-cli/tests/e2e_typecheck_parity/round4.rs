use super::*;
use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

fn factories(value_type: AttributeType) -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".into(),
        schemas: vec![
            ResourceSchema::new("test.Sink")
                .attribute(AttributeSchema::optional("value", value_type).build())
                .validate()
                .unwrap(),
            ResourceSchema::new("test.Source")
                .attribute(
                    AttributeSchema::provider_populated("flag", AttributeType::bool()).build(),
                )
                .validate()
                .unwrap(),
        ],
    })]
}

fn diagnostics(
    value_type: AttributeType,
    fixture: &TempDir,
    file: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories(value_type)),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(loaded), vec!["awsccmock".into()]);
    lsp_diagnostics(&engine, fixture, file)
        .into_iter()
        .filter(|d| {
            matches!(
                d.severity,
                Some(DiagnosticSeverity::WARNING | DiagnosticSeverity::ERROR)
            )
        })
        .collect()
}

#[test]
fn issue_4276_round4_union_local_primary_keeps_resource_error() {
    // main emits both local diagnostics for either alternative. It also
    // retains the provider rejection with SubnetId; the Bool case gained
    // that finding in round 2. Neither may lose the resource-level error.
    for other in [
        AttributeType::refined_string(
            Some(carina_core::schema::TypeIdentity::bare("SubnetId")),
            None,
            None,
            None,
        ),
        AttributeType::bool(),
    ] {
        for name in ["bad", "vpc-1"] {
            let ty = AttributeType::union(vec![
                AttributeType::struct_(
                    "Named",
                    vec![
                        StructField::optional(
                            "name",
                            AttributeType::refined_string(
                                Some(carina_core::schema::TypeIdentity::bare("VpcId")),
                                None,
                                None,
                                None,
                            ),
                        )
                        .build(),
                        StructField::required("size", AttributeType::int()).build(),
                    ],
                ),
                other.clone(),
            ]);
            let fixture = write_fixture(&[
                ("providers.crn", "provider awsccmock {}\n"),
                (
                    "main.crn",
                    &format!("awsccmock.test.Sink {{\n  value = {{ name = \"{name}\" }}\n}}\n"),
                ),
            ]);
            let actual = diagnostics(ty, &fixture, "main.crn");
            let mut expected = vec![
                (
                    "Required attribute 'size' is missing",
                    Some(DiagnosticSeverity::WARNING),
                    Range::new(Position::new(1, 2), Position::new(1, 7)),
                ),
                (
                    "Required attribute 'size' is missing",
                    Some(DiagnosticSeverity::ERROR),
                    Range::new(Position::new(0, 0), Position::new(0, 19)),
                ),
            ];
            if name == "bad" {
                expected.push((
                    "awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'",
                    Some(DiagnosticSeverity::ERROR),
                    Range::new(Position::new(1, 2), Position::new(1, 7)),
                ));
            }
            assert_eq!(
                actual
                    .iter()
                    .map(|d| (d.message.as_str(), d.severity, d.range))
                    .collect::<Vec<_>>(),
                expected,
                "other={other:?}, name={name}"
            );
        }
    }
}

fn nested_module_if(container: &str) -> (TempDir, AttributeType) {
    let leaf = AttributeType::refined_string(None, None, Some((None, Some(5))), None);
    let (ty, value) = match container {
        "struct" => (
            AttributeType::struct_("Named", vec![StructField::optional("name", leaf).build()]),
            "{ name = if flag { \"aaaaaa\" } else { \"bbbbbbb\" } }",
        ),
        "map" => (
            AttributeType::map(leaf),
            "{ name = if flag { \"aaaaaa\" } else { \"bbbbbbb\" } }",
        ),
        "list" => (
            AttributeType::list(leaf),
            "[if flag { \"aaaaaa\" } else { \"bbbbbbb\" }]",
        ),
        _ => panic!("unknown test container: {container}"),
    };
    let fixture = write_fixture(&[
        ("providers.crn", "provider awsccmock {}\n"),
        (
            "main.crn",
            "let gate = awsccmock.test.Source {}\nlet child = use { source = './child' }\nchild { flag = gate.flag }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(
        fixture.path().join("child/arguments.crn"),
        "arguments { flag: Bool }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("child/main.crn"),
        format!("awsccmock.test.Sink {{\n  value = {value}\n}}\n"),
    )
    .unwrap();
    (fixture, ty)
}

#[test]
fn issue_4276_round4_nested_module_if_cli_rejects_runtime_condition_before_validation() {
    for container in ["list", "struct", "map"] {
        let (fixture, ty) = nested_module_if(container);
        let actual = cli_diagnostics(factories(ty), &fixture);
        assert_eq!(
            actual,
            vec![
                "Module resolution error: module 'child' (child/main.crn) at line 2: if condition depends on a runtime value; condition must be statically known at parse time",
            ],
            "container={container}"
        );
    }
}

#[test]
fn issue_4276_round4_list_module_if_lsp_renders_each_branch_once() {
    assert_nested_module_if_lsp("list");
}

#[test]
fn issue_4276_round4_struct_module_if_lsp_renders_each_branch_once() {
    assert_nested_module_if_lsp("struct");
}

#[test]
fn issue_4276_round4_map_module_if_lsp_renders_each_branch_once() {
    assert_nested_module_if_lsp("map");
}

fn assert_nested_module_if_lsp(container: &str) {
    // main defers these nested branches without findings. The shared walk
    // checks both, preserving one precisely located finding per branch.
    let (fixture, ty) = nested_module_if(container);
    let actual = diagnostics(ty, &fixture, "child/main.crn");
    let (start, end) = match container {
        "struct" => (12, 16),
        "map" => (2, 7),
        _ => (11, 50),
    };
    let expected = [("aaaaaa", 6), ("bbbbbbb", 7)].map(|(value, len)| {
        let message = match container {
            "struct" => format!("Invalid value '{value}': length {len} is outside allowed range unbounded..=5"),
            "map" => format!("Map value for key 'name': Invalid value '{value}' for 'value': length {len} is outside allowed range unbounded..=5"),
            _ => format!("Invalid value '{value}' for 'value[0]': length {len} is outside allowed range unbounded..=5"),
        };
        (message, Some(DiagnosticSeverity::WARNING), Range::new(Position::new(1, start), Position::new(1, end)))
    });
    assert_eq!(
        actual
            .iter()
            .map(|d| (d.message.clone(), d.severity, d.range))
            .collect::<Vec<_>>(),
        expected,
        "container={container}"
    );
}
