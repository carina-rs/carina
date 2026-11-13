use carina_core::{
    binding_index::BindingIndex,
    provider::{
        BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint,
        LoadedProviderSnapshot, ProviderFactory, ProviderInitialization, ProviderResult,
    },
    schema::{
        AttributeSchema, AttributeType, PatternMatch, PatternSpec, ResourceSchema, SchemaRegistry,
        StringIdentityLayer, StringMemberConstraints, TypeIdentity, ValidatedProviderSchemaWitness,
        ValidatedResourceSchema,
    },
};
use std::{path::Path, sync::Arc};

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
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
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
                    AttributeSchema::provider_populated("other_id", account("aws.OtherId")).build(),
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

pub fn snapshot() -> LoadedProviderSnapshot {
    carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(vec![Box::new(Factory)]),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    )
}

pub fn directory(main: &str, exports: &str) -> tempfile::TempDir {
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

pub fn validate(dir: &Path, snapshot: &LoadedProviderSnapshot) -> Result<(), String> {
    let context = snapshot.provider_context().as_provider_context();
    let schemas = snapshot.schema_registry();
    let mut loaded = carina_core::config_loader::load_configuration_with_config(
        &carina_core::paths::ResolvedPath::new(dir).unwrap(),
        context,
        schemas,
    )?;
    carina_core::validation::deferred_types::with_session_for_test(
        snapshot.provider_context(),
        |session| loaded.parsed.resolve_type_exprs_report(session),
    )
    .map_err(|e| e.to_string())?;
    let parsed = loaded.parsed.parsed();
    let bindings = BindingIndex::from_parsed(parsed, schemas, context);
    carina_core::validation::validate_resources(
        parsed,
        schemas,
        &std::collections::HashSet::from(["awscc".into()]),
        context,
        &mut Vec::new(),
    )?;
    carina_core::validation::validate_attribute_param_ref_types_with_bindings(
        &parsed.attribute_params,
        &bindings,
    )?;
    carina_core::validation::validate_export_param_ref_types_with_bindings(
        &parsed.export_params,
        &bindings,
    )
    .map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    carina_core::validation::validate_user_function_ref_types_with_bindings(parsed, &bindings)?;
    carina_core::validation::validate_attribute_params(
        &parsed.attribute_params,
        snapshot.provider_context(),
    )?;
    use carina_core::parser::ExportParamLike;
    for export in &parsed.export_params {
        let expression = export.judgement().map_err(|e| e.to_string())?;
        if export.type_expr_opt().is_none() {
            expression.static_type().map_err(|e| e.to_string())?;
        }
        if let (Some(ty), Some(value)) = (export.type_expr_opt(), export.value()) {
            carina_core::parser::retained::admit_expression(expression, ty)
                .map_err(|e| e.to_string())?;
            carina_core::validation::typecheck::check_type_expr_value(
                ty,
                value,
                snapshot.provider_context(),
                expression,
            )
            .require_provider_schemas()
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub const READ_CASES: &[(&str, &str)] = &[
    (
        "let a = awscc.organizations.Account {}\nlet b = awscc.organizations.LooserAccount {}",
        "exports { ids = [a.account_id, b.account_id] }\n",
    ),
    (
        "let a = awscc.organizations.Account {}\nlet b = awscc.organizations.LooserAccount {}",
        "exports { ids = { x = a.account_id, y = b.account_id } }\n",
    ),
    (
        "let a = awscc.organizations.Account {}\nlet b = awscc.organizations.LooserAccount {}",
        "exports { id = a.account_id ?? b.account_id }\n",
    ),
    (
        "let a = awscc.organizations.Account {}\nlet b = awscc.organizations.LooserAccount {}",
        "exports { ids: list(aws.AccountId) = [a.account_id, b.account_id] }\n",
    ),
    (
        "let a = awscc.organizations.Account {}\nlet b = awscc.organizations.LooserAccount {}",
        "exports { ids: map(aws.AccountId) = { x = a.account_id, y = b.account_id } }\n",
    ),
];

pub const CASES: &[(&str, &str)] = &[
    (
        "let id = registry_prod.account_id ?? registry_prod.looser_account_id",
        "exports { account: aws.AccountId = id }\n",
    ),
    (
        "",
        "exports { accounts: map(aws.AccountId) = { x = registry_prod.account_id, y = registry_prod.looser_account_id, z = registry_prod.account_id, w = registry_prod.looser_account_id } }\n",
    ),
    (
        "let ids = [registry_prod.account_id, registry_prod.looser_account_id, registry_prod.account_id, registry_prod.looser_account_id]",
        "exports { accounts: list(aws.AccountId) = ids }\n",
    ),
    (
        "fn keep(ids: list(aws.AccountId)): list(aws.AccountId) { ids }\nlet ids = keep([registry_prod.account_id, registry_prod.looser_account_id, registry_prod.account_id, registry_prod.looser_account_id])",
        "exports { accounts: list(aws.AccountId) = ids }\n",
    ),
    (
        "",
        "exports { accounts: map(aws.AccountId) = { x = registry_prod.account_id } }\n",
    ),
    (
        "",
        "exports { accounts: list(aws.AccountId) = [registry_prod.account_id] }\n",
    ),
    (
        "fn keep(id: aws.AccountId): aws.AccountId { id }\nlet account = keep(registry_prod.account_id)",
        "exports { accounts: map(aws.AccountId) = { x = account } }\n",
    ),
    (
        "fn account(): aws.AccountId { registry_prod.account_id }",
        "exports { accounts: list(aws.AccountId) = [account()] }\n",
    ),
    (
        "let account = registry_prod.account_id\nattributes { account: aws.AccountId = account }",
        "exports { account: aws.AccountId = account }\n",
    ),
];
