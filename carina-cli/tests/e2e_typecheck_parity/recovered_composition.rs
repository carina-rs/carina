use super::*;
use carina_core::schema::{
    PatternMatch, PatternSpec, StringIdentityLayer, StringMemberConstraints, TypeIdentity,
};

fn named(name: &str) -> AttributeType {
    AttributeType::refined_string(Some(TypeIdentity::from_dotted(name)), None, None, None)
}

fn registry(value: AttributeType) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("value", value).build())
            .validate()
            .unwrap(),
    );
    schemas
}

#[derive(Debug, Clone, Copy)]
enum CliPhase {
    Expression,
    ResourceAdmission,
}

fn analyze(
    schemas: &SchemaRegistry,
    value: &str,
    expected_phase: CliPhase,
) -> (Vec<String>, Vec<String>) {
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("sources.crn", "let a = mock.Source { name = 'abc' }\n"),
        (
            "main.crn",
            &format!("let sink = mock.Sink {{ value = {value} }}\n"),
        ),
    ]);
    let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories_for(schemas)),
        Default::default(),
    );
    for error in &report.errors {
        assert!(
            matches!(
                (expected_phase, error),
                (
                    CliPhase::Expression,
                    carina_cli::error::AppError::ExpressionType(_)
                ) | (
                    CliPhase::ResourceAdmission,
                    carina_cli::error::AppError::ResourceAdmission(_)
                )
            ),
            "{value}: expected {expected_phase:?}, got {error:?}"
        );
    }
    let cli = report
        .errors
        .iter()
        .map(|error| carina_cli::error::format_error_with_source_chain(error))
        .map(|message| message.replace(&fixture.path().display().to_string(), "<fixture>"))
        .collect();
    let engine = engine_with_schemas(schemas.clone());
    let lsp = lsp_diagnostics(&engine, &fixture, "main.crn")
        .into_iter()
        .filter(|diagnostic| !diagnostic.message.starts_with("Unused let binding"))
        .map(|diagnostic| diagnostic.message)
        .collect();
    for sibling in ["sources.crn", "providers.crn", "backend.crn"] {
        assert!(
            lsp_diagnostics(&engine, &fixture, sibling)
                .iter()
                .all(
                    |diagnostic| diagnostic.message.starts_with("Unused let binding")
                        || diagnostic.severity == Some(DiagnosticSeverity::HINT)
                )
        );
    }
    (cli, lsp)
}

// The CLI/LSP differences pinned here predate this change.
fn assert_main_plus_reference(
    schemas: &SchemaRegistry,
    value: &str,
    literal: (Vec<String>, Vec<String>),
    main: (Vec<String>, Vec<String>),
    reference: &str,
) {
    assert_eq!(
        analyze(
            schemas,
            &value.replace("a.name", "'abc'"),
            CliPhase::ResourceAdmission
        ),
        literal,
        "main literal control {value}"
    );
    let (mut expected_cli, expected_lsp) = main;
    expected_cli.push(format!("mock.Sink.sink: {reference}"));
    let actual = analyze(schemas, value, CliPhase::ResourceAdmission);
    assert_eq!(actual.0, expected_cli, "CLI {value}");
    assert_eq!(actual.1, expected_lsp, "LSP {value}");
}

// An earlier phase fails first; this behavior predates this change.
fn assert_early_phase(
    schemas: &SchemaRegistry,
    value: &str,
    literal: (Vec<String>, Vec<String>),
    main: (Vec<String>, Vec<String>),
) {
    assert_eq!(
        analyze(
            schemas,
            &value.replace("a.name", "'abc'"),
            CliPhase::ResourceAdmission
        ),
        literal,
        "main literal control {value}"
    );
    assert_eq!(
        analyze(schemas, value, CliPhase::Expression),
        main,
        "earlier phase {value}"
    );
}

const REFERENCE_A: &str =
    "cannot assign String to 'value': expected mock.A, got String (from a.name)";
const EARLY_KIND: &str = "expected mock.A, got Int";

fn named_main(lsp: Vec<String>) -> (Vec<String>, Vec<String>) {
    (
        vec![format!("<fixture>/main.crn at line 1\n{EARLY_KIND}"); 2],
        lsp,
    )
}

fn literal(ordinary: &str, lsp: &str) -> (Vec<String>, Vec<String>) {
    (
        vec![format!("mock.Sink.sink: {ordinary}")],
        vec![lsp.into()],
    )
}

#[test]
fn struct_kind_error_keeps_main_early_phase_diagnostics_in_both_orders() {
    let schemas = registry(AttributeType::struct_(
        "Record",
        vec![
            StructField::required("a_ref", named("mock.A")).build(),
            StructField::required("b_lit", named("mock.A")).build(),
        ],
    ));
    for value in [
        "{ a_ref = a.name\n b_lit = 123 }",
        "{ b_lit = 123\n a_ref = a.name }",
    ] {
        assert_early_phase(
            &schemas,
            value,
            literal(
                "Struct field 'b_lit': Type mismatch: expected mock.A, got Int",
                "Type mismatch: expected mock.A, got Int",
            ),
            named_main(vec![
                EARLY_KIND.into(),
                EARLY_KIND.into(),
                REFERENCE_A.into(),
                "Type mismatch: expected mock.A, got Int".into(),
            ]),
        );
    }
}

#[test]
fn recovered_list_preserves_main_sibling_error_in_both_orders() {
    let schemas = registry(AttributeType::list(named("mock.A")));
    for value in ["[a.name, 123]", "[123, a.name]"] {
        let index = usize::from(value.starts_with("[a.name"));
        let cli = format!("List item at index {index}: Type mismatch: expected mock.A, got Int");
        let lsp = format!("Type mismatch: expected mock.A, got Int for 'value[{index}]'");
        assert_main_plus_reference(
            &schemas,
            value,
            literal(&cli, &lsp),
            (
                vec![format!("mock.Sink.sink: {cli}")],
                vec![REFERENCE_A.into(), lsp],
            ),
            REFERENCE_A,
        );
    }
}

#[test]
fn map_kind_error_keeps_main_early_phase_diagnostics_in_both_orders() {
    let schemas = registry(AttributeType::map(named("mock.A")));
    for value in [
        "{ a_ref = a.name\n b_lit = 123 }",
        "{ b_lit = 123\n a_ref = a.name }",
    ] {
        let ordinary = "Map value for key 'b_lit': Type mismatch: expected mock.A, got Int";
        let mut lsp = vec![EARLY_KIND.into(), EARLY_KIND.into(), REFERENCE_A.into()];
        if value.starts_with("{ b_lit") {
            lsp.push(ordinary.into());
        }
        assert_early_phase(
            &schemas,
            value,
            literal(ordinary, ordinary),
            named_main(lsp),
        );
    }
}

#[test]
fn map_of_struct_kind_error_keeps_main_early_phase_diagnostics_in_both_orders() {
    let schemas = registry(AttributeType::map(AttributeType::struct_(
        "Record",
        vec![
            StructField::required("x", named("mock.A")).build(),
            StructField::required("y", named("mock.A")).build(),
        ],
    )));
    for value in [
        "{ k = { x = a.name\n y = 123 } }",
        "{ k = { y = 123\n x = a.name } }",
    ] {
        let ordinary =
            "Map value for key 'k': Struct field 'y': Type mismatch: expected mock.A, got Int";
        let mut lsp = vec![EARLY_KIND.into(), EARLY_KIND.into(), REFERENCE_A.into()];
        if value.starts_with("{ k = { y") {
            lsp.push(ordinary.into());
        }
        assert_early_phase(
            &schemas,
            value,
            literal(ordinary, ordinary),
            named_main(lsp),
        );
    }
}

fn member_pattern() -> AttributeType {
    AttributeType::string_member(
        StringIdentityLayer::new(Some(TypeIdentity::from_dotted("mock.A")), None, None, None),
        StringMemberConstraints::new(Some(PatternSpec::new("abc", PatternMatch::Full)), None),
    )
}

const CLI_PATTERN: &str = "Invalid value 'bad' for 'value' (mock.A (slot member constraint)): does not match required pattern /abc/ (Full)";
const LSP_PATTERN: &str = "Invalid mock.A (slot member constraint) value 'bad': does not match required pattern /abc/ (Full)";

#[test]
fn recovered_struct_admission_keeps_pattern_sibling_in_both_orders() {
    let schemas = registry(AttributeType::struct_(
        "Record",
        vec![
            StructField::required("a_ref", named("mock.A")).build(),
            StructField::required("b_lit", member_pattern()).build(),
        ],
    ));
    let ordinary = format!("Struct field 'b_lit': {CLI_PATTERN}");
    for value in [
        "{ a_ref = a.name\n b_lit = 'bad' }",
        "{ b_lit = 'bad'\n a_ref = a.name }",
    ] {
        assert_main_plus_reference(
            &schemas,
            value,
            literal(&ordinary, LSP_PATTERN),
            (
                vec![format!("mock.Sink.sink: {ordinary}")],
                vec![REFERENCE_A.into(), LSP_PATTERN.into()],
            ),
            REFERENCE_A,
        );
    }
}

#[test]
fn recovered_map_admission_keeps_pattern_sibling_in_both_orders() {
    let schemas = registry(AttributeType::map(member_pattern()));
    let ordinary = format!("Map value for key 'b_lit': {CLI_PATTERN}");
    for value in [
        "{ a_ref = a.name\n b_lit = 'bad' }",
        "{ b_lit = 'bad'\n a_ref = a.name }",
    ] {
        let mut lsp = vec![REFERENCE_A.into()];
        if value.starts_with("{ b_lit") {
            lsp.push(ordinary.clone());
        }
        assert_main_plus_reference(
            &schemas,
            value,
            literal(&ordinary, &ordinary),
            (vec![format!("mock.Sink.sink: {ordinary}")], lsp),
            REFERENCE_A,
        );
    }
}

#[test]
fn recovered_map_of_struct_admission_keeps_pattern_sibling_in_both_orders() {
    let schemas = registry(AttributeType::map(AttributeType::struct_(
        "Record",
        vec![
            StructField::required("x", named("mock.A")).build(),
            StructField::required("y", member_pattern()).build(),
        ],
    )));
    let ordinary = format!("Map value for key 'k': Struct field 'y': {CLI_PATTERN}");
    for value in [
        "{ k = { x = a.name\n y = 'bad' } }",
        "{ k = { y = 'bad'\n x = a.name } }",
    ] {
        let mut lsp = vec![REFERENCE_A.into()];
        if value.starts_with("{ k = { y") {
            lsp.push(ordinary.clone());
        }
        assert_main_plus_reference(
            &schemas,
            value,
            literal(&ordinary, &ordinary),
            (vec![format!("mock.Sink.sink: {ordinary}")], lsp),
            REFERENCE_A,
        );
    }
}

fn union_main(first: &str, second: &str) -> Vec<String> {
    vec![
        format!("cannot assign String to 'value': expected {first}, got String (from a.name)"),
        format!(
            "No union alternative accepts this value: List item at index 0: Type mismatch: expected {first}, got String; or List item at index 0: Type mismatch: expected {second}, got String"
        ),
    ]
}

#[test]
fn recovered_union_list_uses_main_selected_arm() {
    for (first, second) in [("mock.A", "mock.B"), ("mock.B", "mock.A")] {
        let schemas = registry(AttributeType::union(vec![
            AttributeType::list(named(first)),
            AttributeType::list(named(second)),
        ]));
        let main_lsp = union_main(first, second);
        let reference = main_lsp[0].clone();
        assert_main_plus_reference(
            &schemas,
            "[a.name]",
            (vec![], vec![]),
            (vec![], main_lsp),
            &reference,
        );
    }
}

#[test]
fn recovered_union_list_all_fail_keeps_main_union_diagnostic() {
    for (first, second) in [("mock.A", "mock.B"), ("mock.B", "mock.A")] {
        let arm = |name, pattern| {
            AttributeType::list(AttributeType::refined_string(
                Some(TypeIdentity::from_dotted(name)),
                Some(PatternSpec::new(pattern, PatternMatch::Full)),
                None,
                None,
            ))
        };
        let schemas = registry(AttributeType::union(vec![
            arm(first, "only-a"),
            arm(second, "only-b"),
        ]));
        let ordinary = format!(
            "No union alternative accepts this value: List item at index 0: Invalid value 'abc' for 'value' ({}): does not match required pattern /only-a/ (Full); or List item at index 0: Invalid value 'abc' for 'value' ({}): does not match required pattern /only-b/ (Full)",
            first.trim_start_matches("mock."),
            second.trim_start_matches("mock.")
        );
        let main_lsp = union_main(first, second);
        let reference = main_lsp[0].clone();
        assert_main_plus_reference(
            &schemas,
            "[a.name]",
            literal(&ordinary, &ordinary),
            (vec![format!("mock.Sink.sink: {ordinary}")], main_lsp),
            &reference,
        );
    }
}
