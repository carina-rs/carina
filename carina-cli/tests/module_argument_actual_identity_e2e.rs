//! Probe module argument identity checking on main, independently of #4233 edges.
//! Directory fixtures exercise provider loading, module expansion, and validate.

use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderError,
    ProviderFactory, ProviderInitialization, ProviderResult,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

struct AwsIdentityFactory;

fn identity(resource: &str) -> AttributeType {
    AttributeType::refined_string(
        Some(TypeIdentity::from_schema_type("aws", resource, "Id")),
        None,
        None,
        None,
    )
}

impl ProviderFactory for AwsIdentityFactory {
    fn name(&self) -> &str {
        "aws"
    }

    fn display_name(&self) -> &str {
        "AWS module argument identity fixture"
    }

    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async {
            Err(ProviderError::internal(
                "validation must not initialize a provider",
            ))
        })
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("ec2.Vpc")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("vpc_id", identity("ec2.Vpc")).build(),
                )
                .with_unique_name_attribute("name"),
            ResourceSchema::new("ec2.SecurityGroup")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(AttributeSchema::required("vpc_id", identity("ec2.Vpc")).build())
                .attribute(
                    AttributeSchema::provider_populated("group_id", identity("ec2.SecurityGroup"))
                        .build(),
                )
                .with_unique_name_attribute("name"),
        ]
        .into_iter()
        .map(|schema| schema.validate().expect("valid identity fixture"))
        .collect()
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            ValidatedConfigSchema::default(),
            Default::default(),
            Vec::new(),
        )
        .expect("valid provider fixture")
    }
}

fn validate_module(argument_type: &str, module_body: &str, actual: &str) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let module = temp.path().join("consumer");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&module).unwrap();
    std::fs::write(root.join("providers.crn"), "provider aws {}\n").unwrap();
    std::fs::write(
        root.join("resources.crn"),
        r#"let vpc = aws.ec2.Vpc { name = "vpc" }
let wrong = aws.ec2.SecurityGroup {
  name = "source"
  vpc_id = vpc.vpc_id
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("main.crn"),
        format!(
            "let consumer = use {{ source = '../consumer' }}\nlet instance = consumer {{ vpc_id = {actual} }}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        format!("arguments {{\n  vpc_id: {argument_type}\n}}\n"),
    )
    .unwrap();
    std::fs::write(module.join("resources.crn"), module_body).unwrap();

    let diagnostics = carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(&root).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(vec![Box::new(AwsIdentityFactory)]),
    );
    // nextest --success-output immediate exposes the exact diagnostics for green probes too.
    println!("argument={argument_type}, actual={actual}\n{diagnostics:#?}");
    diagnostics
}

const RESOURCE_SINK: &str = r#"let inside = aws.ec2.SecurityGroup {
  name = "inside"
  vpc_id = vpc_id
}
"#;

#[test]
fn module_argument_declared_x_rejects_actual_identity_y() {
    let control = validate_module("aws.ec2.Vpc.Id", RESOURCE_SINK, "vpc.vpc_id");
    assert!(
        control.is_empty(),
        "matching identity control: {control:#?}"
    );

    let diagnostics = validate_module("aws.ec2.Vpc.Id", RESOURCE_SINK, "wrong.group_id");
    assert!(
        diagnostics.iter().any(|error| {
            error.contains("module call 'instance': argument 'vpc_id'")
                && error.contains("expected aws.ec2.Vpc.Id")
                && error.contains("got aws.ec2.SecurityGroup.Id")
                && error.contains("wrong.group_id")
        }),
        "the formal X must not hide actual Y at the call boundary: {diagnostics:#?}"
    );
}

#[test]
fn module_argument_plain_string_rejects_identity_y_at_resource_sink_x() {
    let diagnostics = validate_module("String", RESOURCE_SINK, "wrong.group_id");
    assert!(
        diagnostics.iter().any(|error| {
            error.contains("aws.ec2.SecurityGroup.inside")
                && error.contains("'vpc_id'")
                && error.contains("expected aws.ec2.Vpc.Id")
                && (error.contains("got String") || error.contains("got aws.ec2.SecurityGroup.Id"))
        }),
        "a plain formal must not admit actual Y into an X resource sink: {diagnostics:#?}"
    );
}

#[test]
fn module_argument_compatible_identity_y_rejects_resource_sink_x() {
    let diagnostics = validate_module("aws.ec2.SecurityGroup.Id", RESOURCE_SINK, "wrong.group_id");
    assert!(
        diagnostics.iter().any(|error| {
            error.contains("aws.ec2.SecurityGroup.inside")
                && error.contains("'vpc_id'")
                && error.contains("expected aws.ec2.Vpc.Id")
                && error.contains("got aws.ec2.SecurityGroup.Id")
        }),
        "accepting Y at the call boundary must not make it assignable to X inside: {diagnostics:#?}"
    );
}
