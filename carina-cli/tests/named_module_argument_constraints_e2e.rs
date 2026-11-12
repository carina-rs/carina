//! carina#4248: named module arguments retain their catalog constraints.
//!
//! Directory fixtures exercise the CLI loader, module expansion, and validation
//! with a native mock whose IAM role name has the real catalog's length bound.

use std::path::PathBuf;

use carina_cli::wiring::LoadedProviderFactory;
use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderResult,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use tempfile::TempDir;

struct AwsTestFactory(&'static str);

impl ProviderFactory for AwsTestFactory {
    fn name(&self) -> &str {
        self.0
    }

    fn display_name(&self) -> &str {
        "AWS (carina#4248 validation stub)"
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
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
        Box::pin(async { panic!("validation tests never instantiate providers") })
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .expect("IAM role name schema witness")
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        // Different reusable definitions belong to separate owners. Keep the
        // same identity and bounds used by the inference assertions below.
        if self.0 == "awscc" {
            return vec![
                ResourceSchema::new("NameSource")
                    .attribute(
                        AttributeSchema::provider_populated(
                            "wide",
                            AttributeType::refined_string(
                                Some(TypeIdentity::from_schema_type("aws", "iam.Role", "Name")),
                                None,
                                Some((Some(1), Some(128))),
                                None,
                            ),
                        )
                        .build(),
                    )
                    .validate()
                    .expect("wide name definition in its own schema owner"),
            ];
        }
        let role_name = AttributeType::refined_string(
            Some(TypeIdentity::from_schema_type("aws", "iam.Role", "Name")),
            None,
            Some((Some(1), Some(64))),
            None,
        );
        vec![
            ResourceSchema::new("iam.Role")
                .attribute(AttributeSchema::required("role_name", role_name.clone()).build())
                .validate()
                .expect("bounded IAM role name schema"),
            ResourceSchema::new("NameSource")
                .attribute(AttributeSchema::provider_populated("narrow", role_name.clone()).build())
                .attribute(
                    AttributeSchema::provider_populated("names", AttributeType::list(role_name))
                        .build(),
                )
                .validate()
                .expect("name source schema with one reusable identity definition"),
        ]
    }
}

struct Fixture {
    _temp: TempDir,
    caller: PathBuf,
}

impl Fixture {
    fn new(module_source: &str, caller_source: &str) -> Self {
        let temp = tempfile::tempdir().expect("fixture directory");
        let module = temp.path().join("module");
        let caller = temp.path().join("caller");
        std::fs::create_dir(&module).expect("module directory");
        std::fs::create_dir(&caller).expect("caller directory");
        std::fs::write(module.join("main.crn"), module_source).expect("module main.crn");
        std::fs::write(caller.join("main.crn"), caller_source).expect("caller main.crn");
        std::fs::write(
            caller.join("providers.crn"),
            "provider aws {}\nprovider awscc {}\n",
        )
        .expect("caller providers.crn");
        Self {
            _temp: temp,
            caller,
        }
    }

    fn validate(&self) -> Vec<String> {
        carina_cli::commands::validate::validate_with_factories(
            &carina_core::paths::ResolvedPath::new(&self.caller).unwrap(),
            LoadedProviderFactory::for_test(vec![
                Box::new(AwsTestFactory("aws")),
                Box::new(AwsTestFactory("awscc")),
            ]),
        )
    }
}

const NAMED_ARGUMENT_MODULE: &str = r#"arguments {
  x: aws.iam.Role.Name = 'ValidName'
}

let role = aws.iam.Role {
  role_name = x
}
"#;

const DEFAULT_CALLER: &str = r#"let role_module = use { source = '../module' }
let instance = role_module {}
"#;

#[test]
fn named_argument_flows_into_same_named_attribute() {
    let diagnostics = Fixture::new(NAMED_ARGUMENT_MODULE, DEFAULT_CALLER).validate();
    assert!(
        diagnostics.is_empty(),
        "a bounded aws.iam.Role.Name argument must flow into the same named attribute:\n{}",
        diagnostics.join("\n"),
    );
}

#[test]
fn caller_value_of_named_type_flows_into_named_argument() {
    // The schema gives source.role_name the bounded aws.iam.Role.Name type;
    // passing this reference exercises the call boundary with a typed source.
    let fixture = Fixture::new(
        NAMED_ARGUMENT_MODULE,
        r#"let role_module = use { source = '../module' }

let source = aws.iam.Role {
  role_name = 'ValidName'
}

let instance = role_module { x = source.role_name }
"#,
    );
    let diagnostics = fixture.validate();
    assert!(
        diagnostics.is_empty(),
        "a caller's aws.iam.Role.Name value must satisfy the same named module argument:\n{}",
        diagnostics.join("\n"),
    );
}

#[test]
fn named_argument_keeps_declared_length_bound() {
    let overlong = "A".repeat(65);
    let fixture = Fixture::new(
        NAMED_ARGUMENT_MODULE,
        &format!(
            "let role_module = use {{ source = '../module' }}\n\
             let instance = role_module {{ x = '{overlong}' }}\n"
        ),
    );
    let diagnostics = fixture.validate();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("argument 'x'")
                && diagnostic.contains("aws.iam.Role.Name")
                && diagnostic.contains("length 65 is outside allowed range 1..=64")
        }),
        "a 65-character argument must fail the declared aws.iam.Role.Name length bound:\n{}",
        diagnostics.join("\n"),
    );
}

#[test]
fn literal_still_validates() {
    let fixture = Fixture::new(
        r#"arguments {
  x: aws.iam.Role.Name = 'ValidName'
}

let role = aws.iam.Role {
  role_name = 'ValidName'
}
"#,
        DEFAULT_CALLER,
    );
    let diagnostics = fixture.validate();
    assert!(
        diagnostics.is_empty(),
        "a valid literal must satisfy the bounded aws.iam.Role.Name attribute:\n{}",
        diagnostics.join("\n"),
    );
}

#[test]
fn computed_access_rejects_heterogeneous_collection_in_directory_validation() {
    for (fallback, expected_error) in [
        (
            "[src.narrow, src.wide]",
            Some(
                "collection element types disagree: `aws.iam.Role.Name (length 1..=64)` vs `aws.iam.Role.Name (length 1..=128)`",
            ),
        ),
        (
            "['ValidName', 1]",
            Some("collection element types disagree: `String` vs `Int`"),
        ),
        ("[src.narrow, src.narrow]", None),
    ] {
        for sink in [
            format!("aws.iam.Role {{ role_name = (src.names ?? {fallback})[1] }}"),
            format!("role_module {{ x = (src.names ?? {fallback})[1] }}"),
        ] {
            let source = format!(
                "let role_module = use {{ source = '../module' }}\nlet narrow = aws.NameSource {{}}\nlet wide = awscc.NameSource {{}}\nlet src = {{ narrow = narrow.narrow, wide = wide.wide, names = narrow.names }}\n{sink}\n"
            );
            let diagnostics = Fixture::new(NAMED_ARGUMENT_MODULE, &source).validate();
            match expected_error {
                Some(expected) => assert!(
                    diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.contains(expected)),
                    "computed access must propagate inference failure for {sink}:\n{}",
                    diagnostics.join("\n"),
                ),
                None => assert!(
                    diagnostics.is_empty(),
                    "a homogeneous bounded fallback must remain valid for {sink}:\n{}",
                    diagnostics.join("\n"),
                ),
            }
        }
    }
}
