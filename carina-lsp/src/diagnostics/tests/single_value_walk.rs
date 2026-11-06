use super::*;
use carina_core::provider::{
    BoxFuture, CanonicalCustomTypeValue, CanonicalProviderConfigAttributes, InitializeOutcomeMint,
    ProviderError, ProviderInitialization, ProviderReadyConfig, ProviderResult,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use tower_lsp::lsp_types::{Position, Range};

struct WalkFactory(ValidatedResourceSchema);

impl ProviderFactory for WalkFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "test"
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![self.0.clone()]
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn validate_config(&self, _: &CanonicalProviderConfigAttributes) -> ProviderResult<()> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
    ) -> ProviderResult<()> {
        if (identity.kind == "SubnetId" && value.as_str().starts_with("subnet-"))
            || (identity.kind == "VpcId" && value.as_str().starts_with("vpc-"))
        {
            return Ok(());
        }
        Err(ProviderError::invalid_input(format!(
            "{} rejects {}",
            identity.kind,
            value.as_str()
        )))
    }
    fn create_provider(
        &self,
        _: Option<&str>,
        _: &ProviderReadyConfig,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { Err(ProviderError::internal("unused test factory")) })
    }
}

fn engine(ty: AttributeType) -> DiagnosticEngine {
    let schema = ResourceSchema::new("test.Widget")
        .attribute(AttributeSchema::optional("value", ty).build())
        .validate()
        .unwrap();
    DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(WalkFactory(schema))]),
        vec!["test".into()],
    )
}

fn remote(name: &str) -> AttributeType {
    AttributeType::refined_string(Some(TypeIdentity::bare(name)), None, None, None)
}

fn value_diagnostics(engine: &DiagnosticEngine, source: &str) -> Vec<Diagnostic> {
    engine
        .analyze(&create_document(source), None)
        .into_iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.severity,
                Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)
            )
        })
        .collect()
}

#[test]
fn issue_4276_round2_a_local_union_retains_only_first_tied_provider_findings() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::struct_(
            "First",
            vec![
                StructField::required("v", remote("VpcId")).build(),
                StructField::required("x", AttributeType::int()).build(),
            ],
        ),
        AttributeType::struct_(
            "Second",
            vec![
                StructField::required("v", remote("SubnetId")).build(),
                StructField::required("y", AttributeType::int()).build(),
            ],
        ),
    ]));
    let diagnostics = value_diagnostics(
        &engine,
        "test.test.Widget {\n  value = { v = \"bad\" }\n}\n",
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Expected one of these forms: ('v' and 'x') or ('v' and 'y')",
            "test rejected `VpcId`: VpcId rejects bad",
        ]
    );
}

#[test]
fn issue_4276_round2_b_provider_only_union_uses_fewest_errors() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::map(remote("SubnetId")),
        AttributeType::struct_(
            "Named",
            vec![
                StructField::required("a", remote("VpcId")).build(),
                StructField::required("b", remote("VpcId")).build(),
            ],
        ),
    ]));
    let diagnostics = value_diagnostics(
        &engine,
        "test.test.Widget {\n  value = { a = \"bad\", b = \"subnet-1\" }\n}\n",
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec!["test rejected `SubnetId`: SubnetId rejects bad",]
    );
}

#[test]
fn issue_4276_round2_c_identified_module_if_reports_local_branch_error() {
    let engine = engine(AttributeType::refined_string(
        Some(TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(5))),
        None,
    ));
    let source = "arguments { flag: Bool }\ntest.test.Widget {\n  value = if flag { \"vpc-1\" } else { \"vpc-123\" }\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Invalid value 'vpc-123' for 'value' (VpcId): length 7 is outside allowed range [, 5]",
        ]
    );
}

#[test]
fn issue_4276_round2_c_module_if_reports_errors_from_both_branches() {
    let engine = engine(AttributeType::refined_string(
        Some(TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(5))),
        None,
    ));
    for both_local in [false, true] {
        let first = if both_local { "bad-long" } else { "bad" };
        let source = format!(
            "arguments {{ flag: Bool }}\ntest.test.Widget {{\n  value = if flag {{ \"{first}\" }} else {{ \"vpc-123\" }}\n}}\n"
        );
        let diagnostics = value_diagnostics(&engine, &source);
        let mut expected = vec![
            "Invalid value 'vpc-123' for 'value' (VpcId): length 7 is outside allowed range [, 5]",
        ];
        if both_local {
            expected.insert(0, "Invalid value 'bad-long' for 'value' (VpcId): length 8 is outside allowed range [, 5]");
        } else {
            expected.push("test rejected `VpcId`: VpcId rejects bad");
        }
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn issue_4276_function_container_uses_authored_findings() {
    let engine = engine(AttributeType::struct_(
        "Settings",
        vec![StructField::optional("name", AttributeType::string()).build()],
    ));
    let source = "let broken = test.test.Widget { value = length(1) }\ntest.test.Widget {\n  value = split(\",\", \"a,b\")\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Invalid expression at line 1: length(): length() argument must be a List, Map, or String, got Int"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 13), Position::new(0, 51))
    );
}

#[test]
fn issue_4276_union_function_uses_authored_findings() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::int(),
        AttributeType::bool(),
    ]));
    let source = "let broken = test.test.Widget { value = length(1) }\ntest.test.Widget {\n  value = split(\",\", \"a,b\")\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Invalid expression at line 1: length(): length() argument must be a List, Map, or String, got Int"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 13), Position::new(0, 51))
    );
}

#[test]
fn issue_4276_coalesce_uses_authored_findings() {
    let engine = engine(AttributeType::int());
    let source = "test.test.Widget {\n  value = length([1]) ?? \"bad\"\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "coalesce default type `String` does not match left type `Int`"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(1, 25), Position::new(1, 30))
    );
    assert_eq!(
        diagnostics[1].message,
        "Type mismatch: expected Int, got String"
    );
    assert_eq!(diagnostics[1].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        diagnostics[1].range,
        Range::new(Position::new(1, 2), Position::new(1, 7))
    );
}

#[test]
fn issue_4276_coalesce_provider_failures_render_once() {
    let engine = engine(remote("Remote"));
    let source = "test.test.Widget {\n  value = secret(\"left\") ?? \"right\"\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let messages = diagnostics
        .iter()
        .map(|d| &d.message)
        .collect::<HashSet<_>>();
    assert_eq!(messages.len(), 1, "{diagnostics:#?}");
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            diagnostic.message,
            "Type mismatch: expected Remote, got String (from (secret) ?? 'right')"
        );
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(1, 2), Position::new(1, 7))
        );
    }
}

#[test]
fn issue_4276_union_provider_failures_render_once() {
    let engine = engine(AttributeType::union(vec![
        remote("First"),
        remote("Second"),
    ]));
    let source = "test.test.Widget {\n  value = secret(\"left\") ?? \"right\"\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Type mismatch: expected First | Second, got String (from (secret) ?? 'right')"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(1, 2), Position::new(1, 7))
    );
}
