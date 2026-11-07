use super::*;

struct BoundaryFactory;

impl ProviderFactory for BoundaryFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Construction boundary mock"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        config: &carina_core::provider::ProviderSession,
        mint: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        TypedRefMockFactory.create_provider(config, mint)
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let mut schemas = TypedRefMockFactory.schemas();
        let named = AttributeType::refined_string(
            Some(TypeIdentity::from_schema_type(
                "mock",
                "test.Boundary",
                "Name",
            )),
            Some(carina_core::schema::PatternSpec::new(
                PATTERN,
                carina_core::schema::PatternMatch::Full,
            )),
            None,
            None,
        );
        schemas.push(
            ResourceSchema::new("test.Boundary")
                .attribute(AttributeSchema::optional("scalar", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("list", AttributeType::list(AttributeType::string()))
                        .build(),
                )
                .attribute(AttributeSchema::optional("named", named.clone()).build())
                .attribute(
                    AttributeSchema::optional("named_list", AttributeType::list(named)).build(),
                )
                .validate()
                .unwrap(),
        );
        schemas
    }
}

fn boundary_factories() -> Vec<LoadedProviderFactory> {
    LoadedProviderFactory::for_test(vec![Box::new(BoundaryFactory)])
}

// origin/main 980fff07f admits these deferred boundaries but does not revisit
// their named rule at plan time. The consumer has only plain String sinks, so
// a resource constraint cannot accidentally stand in for a boundary check.
fn assert_unchecked_boundary_plan(expression: &str, ty: &str, attribute: &str) {
    for module in [false, true] {
        let scenario = Scenario::new("");
        fs::create_dir(scenario.project.path().join("child")).unwrap();
        let (body, argument) = if module {
            (
                format!(
                    "arguments {{ value: {ty} }}\nmock.test.Boundary {{ {attribute} = value }}\n"
                ),
                expression.to_owned(),
            )
        } else {
            (
                format!(
                    "arguments {{ input: String }}\nlet checked = {}\nattributes {{ checked: {ty} = checked }}\nmock.test.Boundary {{ {attribute} = checked }}\n",
                    expression.replace("producer.typed_out", "input")
                ),
                "producer.typed_out".to_owned(),
            )
        };
        fs::write(scenario.project.path().join("child/main.crn"), body).unwrap();
        fs::write(scenario.project.path().join("consumer.crn"), format!("let template = use {{ source = './child' }}\nlet child = template {{ {} = {argument} }}\n", if module { "value" } else { "input" })).unwrap();
        let validate = carina_cli::commands::validate::validate_with_factories(
            &carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap(),
            boundary_factories(),
        );
        assert!(
            validate.is_empty(),
            "{expression}, module={module}: {validate:#?}"
        );
        scenario.seed_output("bad-id");
        let fixture = build_plan_from_fixture_path_using(
            &TestCommandRuntime::new(),
            scenario.project.path(),
            |_| boundary_factories(),
        )
        .unwrap();
        assert!(
            fixture.plan.errors().is_empty(),
            "{expression}, module={module}: {:?}",
            fixture.plan.errors()
        );
        let resolved = fixture
            .desired_resources
            .iter()
            .find(|resource| resource.id.resource_type == "test.Boundary")
            .unwrap();
        let expected = Value::Concrete(ConcreteValue::String("bad-id".into()));
        let expected = if attribute == "list" {
            Value::Concrete(ConcreteValue::List(vec![expected]))
        } else {
            expected
        };
        assert_eq!(
            resolved.attributes[attribute], expected,
            "the deferred output must actually resolve"
        );
        carina_state::BackendLock::local_default()
            .save(&carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap())
            .unwrap();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_plan_with_factories(
                &TestCommandRuntime::new(),
                scenario.project.path(),
                boundary_factories(),
            ));
        assert!(
            result.unwrap(),
            "main accepts the resolved value at this boundary"
        );
    }
}

#[test]
fn issue_4282_round2_plan_preserves_unchecked_interpolation_boundaries_on_main() {
    for (expression, ty, attribute) in [
        (
            r#""${''}${producer.typed_out}""#,
            "mock.test.Boundary.Name",
            "scalar",
        ),
        (
            r#"["${''}${producer.typed_out}"]"#,
            "list(mock.test.Boundary.Name)",
            "list",
        ),
    ] {
        assert_unchecked_boundary_plan(expression, ty, attribute);
    }
}

#[test]
fn issue_4282_round2_plan_builtin_boundaries_match_interpolation() {
    for (expression, ty, attribute) in [
        (
            "join('', [producer.typed_out])",
            "mock.test.Boundary.Name",
            "scalar",
        ),
        (
            "split(',', producer.typed_out)",
            "list(mock.test.Boundary.Name)",
            "list",
        ),
        (
            "[join('', [producer.typed_out])]",
            "list(mock.test.Boundary.Name)",
            "list",
        ),
    ] {
        assert_unchecked_boundary_plan(expression, ty, attribute);
    }
}

#[test]
fn issue_4282_round2_plan_enforces_resolved_named_resource_rules() {
    for (attribute, expression) in [
        ("named", "join('', [producer.typed_out])"),
        ("named_list", "split(',', producer.typed_out)"),
    ] {
        let scenario = Scenario::new("");
        fs::write(
            scenario.project.path().join("consumer.crn"),
            format!("mock.test.Boundary {{ {attribute} = {expression} }}\n"),
        )
        .unwrap();
        let validate = carina_cli::commands::validate::validate_with_factories(
            &carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap(),
            boundary_factories(),
        );
        assert!(validate.is_empty(), "{validate:?}");
        scenario.seed_output("bad-id");
        let fixture = build_plan_from_fixture_path_using(
            &TestCommandRuntime::new(),
            scenario.project.path(),
            |_| boundary_factories(),
        )
        .unwrap();
        assert_eq!(
            fixture.plan.errors().len(),
            1,
            "{:?}",
            fixture.plan.errors()
        );
        assert!(
            matches!(&fixture.plan.errors()[0].kind, PlanErrorKind::ResolvedValueConstraint { message, .. } if message.as_str().contains("pattern")),
            "{:?}",
            fixture.plan.errors()
        );
        carina_state::BackendLock::local_default()
            .save(&carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap())
            .unwrap();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_plan_with_factories(
                &TestCommandRuntime::new(),
                scenario.project.path(),
                boundary_factories(),
            ));
        assert!(
            matches!(result, Err(carina_cli::error::AppError::Validation(message)) if message == "plan failed with 1 error(s)")
        );
    }
}
