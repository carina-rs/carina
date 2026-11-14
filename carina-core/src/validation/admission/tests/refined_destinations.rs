//! Directory-backed forwarding boundaries. Authored annotations name an
//! identity; the final schema slot owns its separate member constraints.
use crate::{
    binding_index::{BindingIndex, BindingValueSource, ResolvedBindings},
    provider::LoadedProviderSnapshot,
    resource::{ConcreteValue, Value, knowledge::KnowledgeContext},
    validation::{
        admission::{Candidate, admit},
        deferred_types::with_session_for_test,
    },
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    Resource,
    Let,
    Export,
    ModuleArgument,
    FunctionParameter,
    FunctionReturn,
    Attributes,
}

pub fn directory(boundary: Boundary, resolved_length: Option<usize>) -> tempfile::TempDir {
    let input =
        resolved_length.map_or_else(|| "source.name".into(), |n| format!("'{}'", "A".repeat(n)));
    let slot = |value: &str| format!("let sink = awscc.MemberSink {{ name = {value} }}\n");
    let (main, child) = match boundary {
        Boundary::Resource => (slot(&input), None),
        // Typed lets are not syntax; a let forwards identity-only value evidence.
        Boundary::Let => (format!("let name = {input}\n{}", slot("name")), None),
        Boundary::FunctionParameter => (
            format!(
                "fn forward(name: aws.Name): aws.Name {{ name }}\n{}",
                slot(&format!("forward({input})"))
            ),
            None,
        ),
        Boundary::FunctionReturn => (
            format!(
                "fn forward(): aws.Name {{ {input} }}\n{}",
                slot("forward()")
            ),
            None,
        ),
        Boundary::Export => (slot(&input), None),
        Boundary::Attributes => (
            format!(
                "attributes {{ name: aws.Name = {input} }}\n{}",
                slot(&input)
            ),
            None,
        ),
        Boundary::ModuleArgument => (
            format!(
                "let forward = use {{ source = './forward' }}\nlet out = forward {{ name = {input} }}\n"
            ),
            Some(format!("arguments {{ name: aws.Name }}\n{}", slot("name"))),
        ),
    };
    let exports = if matches!(boundary, Boundary::Export) {
        format!("exports {{ name: aws.Name = {input} }}\n")
    } else {
        "exports {}\n".into()
    };
    let dir = fixture::directory(
        &format!("let source = awscc.MemberSource {{}}\n{main}"),
        &exports,
    );
    if let Some(child) = child {
        std::fs::create_dir(dir.path().join("forward")).unwrap();
        std::fs::write(dir.path().join("forward/main.crn"), child).unwrap();
    }
    dir
}

pub fn check(
    boundary: Boundary,
    snapshot: &LoadedProviderSnapshot,
    length: Option<usize>,
) -> Result<(), String> {
    let dir = directory(boundary, None);
    let root = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    let context = snapshot.provider_context().as_provider_context();
    let schemas = snapshot.schema_registry();
    let mut loaded = crate::config_loader::load_configuration_with_config(&root, context, schemas)?;
    with_session_for_test(snapshot.provider_context(), |session| {
        loaded.parsed.resolve_type_exprs_report(session)
    })
    .map_err(|e| e.to_string())?;
    let root_bindings = BindingIndex::from_parsed(loaded.parsed.parsed(), schemas, context);
    crate::validation::validate_user_function_ref_types_with_bindings(
        loaded.parsed.parsed(),
        &root_bindings,
    )?;
    let parsed = with_session_for_test(snapshot.provider_context(), |session| {
        crate::module_resolver::resolve_modules_with_config(loaded.parsed, &root, session, schemas)
    })
    .map_err(|e| e.to_string())?;
    crate::validation::validate_resources(
        &parsed,
        schemas,
        &HashSet::from(["awscc".into()]),
        context,
        &mut Vec::new(),
    )?;
    let bindings = BindingIndex::from_parsed(&parsed, schemas, context);
    crate::validation::validate_attribute_param_ref_types_with_bindings(
        &parsed.attribute_params,
        &bindings,
    )?;
    crate::validation::validate_export_param_ref_types_with_bindings(
        &parsed.export_params,
        &bindings,
    )
    .map_err(|e| {
        e.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let expanded = parsed
        .clone()
        .into_expanded(schemas, context)
        .map_err(|e| e.to_string())?;
    let sink = expanded
        .resources
        .iter()
        .find(|r| r.id.resource_type == "MemberSink")
        .expect("the actual member-bearing slot exists");
    let destination = schemas
        .resource_attribute_destination(&sink.id, "name", context)
        .unwrap();
    with_session_for_test(snapshot.provider_context(), |session| {
        use crate::parser::ExportParamLike;
        let value = match boundary {
            Boundary::Export => expanded.export_params[0].value().unwrap(),
            Boundary::Attributes => expanded.attribute_params[0]
                .value
                .as_ref()
                .unwrap()
                .value()
                .unwrap(),
            _ => &sink.attributes["name"],
        };
        let (candidate, context) = Candidate::validation_classified(
            value
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification(),
            Some(&bindings),
            session,
        );
        let pending = admit(&destination, candidate, &context)
            .map_err(|e| e.resolution_error().to_string())?
            .ready()
            .expect_err("the source is unknown at validate time");
        assert!(
            !pending.sites().is_empty(),
            "{boundary:?}: reference resolution must remain pending"
        );
        if let Some(length) = length {
            let mut resolved = ResolvedBindings::default();
            resolved.set(
                "source",
                HashMap::from([(
                    "name".into(),
                    Value::Concrete(ConcreteValue::String("A".repeat(length).into())),
                )]),
                BindingValueSource::Local,
            );
            pending
                .discharge(&resolved, session)
                .map_err(|e| e.resolution_error().to_string())?;
        }
        Ok(())
    })
}

mod fixture {
    use crate::{
        provider::{
            BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint,
            LoadedProviderSnapshot, ProviderFactory, ProviderInitialization, ProviderResult,
        },
        schema::{
            AttributeSchema, AttributeType, PatternMatch, PatternSpec, ResourceSchema,
            SchemaRegistry, StringIdentityLayer, StringMemberConstraints, TypeIdentity,
            ValidatedProviderSchemaWitness, ValidatedResourceSchema,
        },
    };
    use std::sync::Arc;

    struct Factory;
    impl ProviderFactory for Factory {
        fn name(&self) -> &str {
            "awscc"
        }
        fn display_name(&self) -> &str {
            "account annotation fixture"
        }

        fn validate_config(
            &self,
            _: &CanonicalProviderConfigAttributes,
            _: &crate::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Ok(())
        }
        fn create_provider(
            &self,
            _: &crate::provider::ProviderSession,
            _: InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
            Box::pin(async { panic!("schema-only fixture") })
        }
        fn schemas(&self) -> Vec<ValidatedResourceSchema> {
            let account = |identity| {
                AttributeType::string_member(
                    StringIdentityLayer::new(
                        Some(TypeIdentity::from_dotted(identity)),
                        None,
                        None,
                        None,
                    ),
                    StringMemberConstraints::new(
                        Some(PatternSpec::new(r"^\d{12}$", PatternMatch::Full)),
                        Some((None, Some(12))),
                    ),
                )
            };
            let looser = AttributeType::string_member(
                StringIdentityLayer::new(
                    Some(TypeIdentity::from_dotted("aws.AccountId")),
                    None,
                    None,
                    None,
                ),
                StringMemberConstraints::new(
                    Some(PatternSpec::new(r"^\d{12}$", PatternMatch::Full)),
                    Some((None, Some(64))),
                ),
            );
            let name_identity = || {
                StringIdentityLayer::new(
                    Some(TypeIdentity::from_dotted("aws.Name")),
                    None,
                    Some((Some(1), Some(100))),
                    None,
                )
            };
            vec![
                ResourceSchema::new("MemberSource")
                    .attribute(
                        AttributeSchema::provider_populated(
                            "name",
                            AttributeType::string_member(name_identity(), Default::default()),
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
                ResourceSchema::new("MemberSink")
                    .attribute(
                        AttributeSchema::optional(
                            "name",
                            AttributeType::string_member(
                                name_identity(),
                                StringMemberConstraints::new(None, Some((Some(1), Some(10)))),
                            ),
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
                ResourceSchema::new("organizations.Account")
                    .attribute(
                        AttributeSchema::provider_populated("account_id", account("aws.AccountId"))
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::provider_populated("other_id", account("aws.OtherId"))
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::provider_populated("looser_account_id", looser.clone())
                            .build(),
                    )
                    .validate()
                    .unwrap(),
                ResourceSchema::new("organizations.LooserAccount")
                    .attribute(AttributeSchema::provider_populated("account_id", looser).build())
                    .validate()
                    .unwrap(),
            ]
        }
        fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
            ValidatedProviderSchemaWitness::new(
                self.schemas(),
                Default::default(),
                Default::default(),
                vec![],
            )
            .unwrap()
        }
    }

    pub(super) fn snapshot() -> LoadedProviderSnapshot {
        crate::provider::loaded_provider_snapshot_from_factories(
            Arc::new(vec![Box::new(Factory)]),
            SchemaRegistry::new(),
            crate::EnvironmentSnapshot::empty(),
        )
    }

    pub(super) fn directory(main: &str, exports: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("providers.crn"), "provider awscc {}\n").unwrap();
        std::fs::write(
            dir.path().join("main.crn"),
            format!("let registry_prod = awscc.organizations.Account {{}}\n{main}\n"),
        )
        .unwrap();
        std::fs::write(dir.path().join("exports.crn"), exports).unwrap();
        dir
    }
}

macro_rules! boundary_test {
    ($name:ident, $boundary:ident) => {
        #[test]
        fn $name() {
            let snapshot = fixture::snapshot();
            let boundary = Boundary::$boundary;
            for length in [None, Some(5), Some(50)] {
                let result = check(boundary, &snapshot, length);
                assert_eq!(
                    result.is_ok(),
                    length != Some(50),
                    "{boundary:?} {length:?}: {result:?}"
                );
                if let Err(error) = result {
                    assert_eq!(
                        error.matches("outside allowed range").count(),
                        1,
                        "{boundary:?}: {error}"
                    );
                }
            }
        }
    };
}
boundary_test!(round15_resource_attribute, Resource);
boundary_test!(round15_let_forwarding, Let);
boundary_test!(round15_export, Export);
boundary_test!(round15_module_argument, ModuleArgument);
boundary_test!(round15_function_parameter, FunctionParameter);
boundary_test!(round15_function_return, FunctionReturn);
boundary_test!(round15_attributes_block, Attributes);
