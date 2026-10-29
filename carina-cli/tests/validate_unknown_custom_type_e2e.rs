//! End-to-end validation regression for carina#3239.
//!
//! Declaring a module argument with an unknown bare custom-type name —
//! whether a typo (`TotallyMadeUpType`) or a renamed-then-removed
//! legacy spelling (`IamOidcProviderArn` — the issue's headline case,
//! where the registered identity is now `aws.iam.OidcProvider.Arn`) —
//! must fail `carina validate` with a clear "unknown custom type" error
//! instead of silently degrading into an untyped string.
//!
//! The check fires at parse time, gated by whether provider custom types are
//! loaded. Production runs through `enrich_provider_context`, which installs
//! `ProviderCustomTypes::Loaded` once schemas have
//! been collected; this test exercises the same CLI surface
//! (`validate_with_factories`) that `carina-cli/tests/nested_module_call_ref_e2e.rs`
//! does, so the fixture covers the real validate path end-to-end —
//! including the [[feedback_directory_scoped_features]] requirement
//! that any DSL-source feature be tested against a real multi-file
//! module directory, not a bare string.

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::Value;
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, legacy_validator,
};
use indexmap::IndexMap;
use std::collections::HashMap;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Minimal awscc provider factory; the fixture only references
// `awscc.ec2.Vpc` so the error surface is the carina#3239 unknown-type
// check itself, not an unrelated "unknown provider" diagnostic.
// ---------------------------------------------------------------------------

struct AwsccTestFactory {
    include_generic_arn: bool,
}

impl ProviderFactory for AwsccTestFactory {
    fn name(&self) -> &str {
        "awscc"
    }
    fn display_name(&self) -> &str {
        "AWSCC (carina#3239 test stub)"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        _type_name: &carina_core::schema::TypeIdentity,
        _value: &carina_core::provider::CanonicalCustomTypeValue,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }
    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }
    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(NoopProvider) as Box<dyn Provider>) })
    }
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            let iam_policy_arn = AttributeType::refined_string_with_validator(
                TypeIdentity::new(Some("aws"), ["iam", "Policy"], "Arn"),
                None,
                None,
                legacy_validator(|_| Ok(())),
                None,
            );

            let generic_arn = AttributeType::refined_string_with_validator(
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn"),
                None,
                None,
                legacy_validator(|_| Ok(())),
                None,
            );

            let bare_token = AttributeType::refined_string_with_validator(
                TypeIdentity::bare("TestToken"),
                None,
                None,
                legacy_validator(|_| Err("bare token rejected by its owner".to_string())),
                None,
            );

            let mut iam_role = ResourceSchema::new("iam.Role")
                .attribute(AttributeSchema::optional("policy_arn", iam_policy_arn).build())
                .attribute(AttributeSchema::optional("bare_token", bare_token).build());
            if self.include_generic_arn {
                iam_role =
                    iam_role.attribute(AttributeSchema::optional("role_arn", generic_arn).build());
            }

            vec![
                ResourceSchema::new("ec2.Vpc")
                    .attribute(
                        AttributeSchema::optional("cidr_block", AttributeType::string()).build(),
                    )
                    .attribute(
                        AttributeSchema::optional("vpc_id", AttributeType::string()).build(),
                    ),
                iam_role,
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "awscc"
    }
    fn read(
        &self,
        id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ReadOutcome>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::provider::ReadOutcome::not_found(id)) })
    }
    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn create(
        &self,
        _id: &carina_core::resource::ResourceId,
        _request: carina_core::provider::CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        Box::pin(async move {
            Ok(attempt
                .issued(
                    carina_core::provider::AppliedIdentifier::new("mock-id")
                        .expect("fixture identifier is non-empty"),
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .succeeded(HashMap::new()))
        })
    }
    fn resume_create(
        &self,
        _request: carina_core::provider::ResumeCreateRequest,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async { panic!("validation fixture never resumes a create") })
    }
    fn update(
        &self,
        id: &carina_core::resource::ResourceId,
        identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        Box::pin(async move {
            Ok(carina_core::provider::UpdateOutcome::Success {
                state: carina_core::provider::AppliedState::new(id, identifier, HashMap::new())
                    .expect("provider input identifier is non-empty"),
            })
        })
    }
    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn required_permissions(
        &self,
        _id: &carina_core::resource::ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn factories() -> Vec<carina_cli::wiring::LoadedProviderFactory> {
    carina_cli::wiring::LoadedProviderFactory::for_test(vec![Box::new(AwsccTestFactory {
        include_generic_arn: false,
    }) as Box<dyn ProviderFactory>])
}

fn factories_with_generic_arn() -> Vec<carina_cli::wiring::LoadedProviderFactory> {
    carina_cli::wiring::LoadedProviderFactory::for_test(vec![Box::new(AwsccTestFactory {
        include_generic_arn: true,
    }) as Box<dyn ProviderFactory>])
}

/// Two-directory fixture: a module that *declares* an unknown
/// custom-type argument, and a root caller. Module is the surface
/// `arguments { ... }` legitimately appears on — the parser check fires
/// on the module's `.crn` files when the caller's validate pass loads
/// the imported module through `module_resolver::load_module`.
fn write_fixture(arg_type: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let argument_value = if arg_type.trim_start().starts_with("list(") {
        "['anything']"
    } else {
        "'anything'"
    };

    std::fs::create_dir(dir.path().join("inner")).unwrap();
    std::fs::create_dir(dir.path().join("caller")).unwrap();

    // The module's `arguments` block carries the offending unknown
    // type. The body is otherwise minimal — the test asserts on the
    // type-name diagnostic, not on any downstream module behavior.
    std::fs::write(
        dir.path().join("inner/main.crn"),
        format!(
            r#"arguments {{
  bad_arg: {arg_type}
}}

let vpc = awscc.ec2.Vpc {{
  cidr_block = '10.0.0.0/16'
}}
"#
        ),
    )
    .unwrap();

    std::fs::write(
        dir.path().join("caller/providers.crn"),
        r#"provider awscc {
  region = "us-east-1"
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("caller/main.crn"),
        format!(
            r#"let inner = use {{ source = '../inner' }}

let m = inner {{
  bad_arg = {argument_value}
}}
"#
        ),
    )
    .unwrap();

    dir
}

/// Headline case from the issue: a clearly-fake PascalCase name in a
/// module argument's type position must be rejected.
#[test]
fn validate_rejects_fake_custom_type_name() {
    let fixture = write_fixture("TotallyMadeUpType");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags
            .iter()
            .any(|d| d.contains("unknown custom type") && d.contains("TotallyMadeUpType")),
        "validate must reject an unknown custom-type name with a clear \
         message; got diagnostics: {:#?}",
        diags
    );
}

/// The actual carina#3239 motivating case: `IamOidcProviderArn` is a
/// legacy spelling whose registered identity has been renamed to
/// `aws.iam.OidcProvider.Arn`. The bare name is *not* registered, so
/// it must be rejected the same as any other unknown name.
#[test]
fn validate_rejects_renamed_legacy_custom_type_name() {
    let fixture = write_fixture("IamOidcProviderArn");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags
            .iter()
            .any(|d| d.contains("unknown custom type") && d.contains("IamOidcProviderArn")),
        "validate must reject a renamed-then-removed custom-type name \
         (carina#3239 headline case); got diagnostics: {:#?}",
        diags
    );
}

/// Reproduces carina#3368: a dotted custom type that is not registered
/// must be rejected instead of silently validating as an untyped string.
/// This also pins the signature-loader boundary: its best-effort snapshot may
/// retain the unresolved type, but the caller-side parameter classifier must
/// resolve it before deciding whether the argument is a handle or a value.
#[test]
fn validate_rejects_fake_dotted_custom_type() {
    let fixture = write_fixture("list(aws.iam.TotallyFake.Arn)");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags.iter().any(|d| {
            d.contains("module inner argument 'bad_arg'")
                && d.contains("unknown custom type")
                && (d.contains("aws.iam.TotallyFake.Arn") || d.contains("TotallyFake"))
        }),
        "validate must reject an unregistered dotted custom type; got \
         diagnostics: {:#?}",
        diags
    );
}

/// Far-away dotted names should fail plainly without a misleading custom-type
/// suggestion.
#[test]
fn validate_rejects_fake_dotted_custom_type_without_bad_suggestion() {
    let fixture = write_fixture("aws.foo.TotallyMadeUp.Xyz");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags.iter().any(|d| {
            d.contains("unknown custom type") && d.contains("aws.foo.TotallyMadeUp.Xyz")
        }),
        "validate must reject the unregistered dotted custom type; got \
         diagnostics: {:#?}",
        diags
    );
    assert!(
        !diags.iter().any(|d| d.contains("aws.iam.Policy.Arn")),
        "far-away unknown custom types must not suggest unrelated registered \
         identities; got diagnostics: {:#?}",
        diags
    );
}

/// A written dotted annotation must name an exact registered identity.
/// `aws.Arn` is wider than the registered `aws.iam.Policy.Arn`, but it
/// is not itself registered, so validation must reject it.
#[test]
fn validate_rejects_wider_bare_provider_dotted_type() {
    let fixture = write_fixture("aws.Arn");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags
            .iter()
            .any(|d| d.contains("unknown custom type") && d.contains("aws.Arn")),
        "validate must reject the unregistered wider dotted custom type \
         `aws.Arn`; got diagnostics: {:#?}",
        diags
    );
}

/// Registered dotted identities such as `aws.iam.Policy.Arn` remain valid;
/// this guards the fix from rejecting provider-scoped custom types wholesale.
#[test]
fn validate_accepts_registered_dotted_custom_type() {
    let fixture = write_fixture("list(aws.iam.Policy.Arn)");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        !diags.iter().any(|d| d.contains("unknown custom type")),
        "validate must accept the registered dotted custom type \
         `aws.iam.Policy.Arn`; got diagnostics: {:#?}",
        diags
    );
}

/// Reproduces carina#3368: snake_case custom-type spelling should point
/// users at the registered dotted identity, not an unregistered bare name.
#[test]
fn validate_snake_case_suggests_dotted_registered_identity() {
    let fixture = write_fixture("iam_policy_arn");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags.iter().any(|d| {
            d.contains("aws.iam.Policy.Arn") && (d.contains("use") || d.contains("suggest"))
        }),
        "validate should suggest the registered dotted identity \
         `aws.iam.Policy.Arn` for `iam_policy_arn`; got diagnostics: \
         {:#?}",
        diags
    );
    assert!(
        !diags.iter().any(|d| d.contains("IamPolicyArn")),
        "validate must not suggest the unregistered bare name \
         `IamPolicyArn`; got diagnostics: {:#?}",
        diags
    );
}

/// Reproduces carina#3377: a typo in a non-final dotted segment must
/// prefer the nearest full registered identity over the generic
/// same-kind identity.
#[test]
fn validate_suggests_full_identity_for_non_final_segment_typo() {
    let fixture = write_fixture("list(aws.iam.Plicy.Arn)");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(
        &caller,
        factories_with_generic_arn(),
    );

    assert!(
        diags.iter().any(|d| {
            d.contains("unknown custom type")
                && d.contains("aws.iam.Plicy.Arn")
                && d.contains("suggestion: use 'aws.iam.Policy.Arn'")
        }),
        "validate should suggest the nearest full dotted identity \
         `aws.iam.Policy.Arn`; got diagnostics: {:#?}",
        diags
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.contains("suggestion: use 'aws.Arn'")),
        "validate must not collapse non-final dotted typos to the generic \
         same-kind identity `aws.Arn`; got diagnostics: {:#?}",
        diags
    );
}

/// Far dotted typos should remain unknown without falling back to a
/// generic same-kind identity.
#[test]
fn validate_no_suggestion_for_far_dotted_typo() {
    let fixture = write_fixture("list(aws.iam.TotallyFake.Arn)");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(
        &caller,
        factories_with_generic_arn(),
    );

    assert!(
        diags.iter().any(|d| {
            d.contains("unknown custom type") && d.contains("aws.iam.TotallyFake.Arn")
        }),
        "validate must reject the unregistered dotted custom type; got \
         diagnostics: {:#?}",
        diags
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.contains("suggestion: use 'aws.Arn'")),
        "validate must not suggest the generic same-kind identity \
         `aws.Arn`; got diagnostics: {:#?}",
        diags
    );
}

/// A distance-1 typo in the final segment should still be corrected by
/// the full-identity edit-distance matcher.
#[test]
fn validate_suggests_for_distance1_final_segment_typo() {
    let fixture = write_fixture("list(aws.iam.Policy.Arnn)");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(
        &caller,
        factories_with_generic_arn(),
    );

    assert!(
        diags.iter().any(|d| {
            d.contains("unknown custom type")
                && d.contains("aws.iam.Policy.Arnn")
                && d.contains("suggestion: use 'aws.iam.Policy.Arn'")
        }),
        "validate should suggest the registered dotted identity \
         `aws.iam.Policy.Arn` for a distance-1 final-segment typo; got \
         diagnostics: {:#?}",
        diags
    );
}

/// Negative control: a built-in DSL custom type (`Ipv4Cidr`) must
/// still be accepted. Guards against the strict check over-firing on
/// the four `carina-core` built-ins, which carry no provider
/// registration and would be the first false-positive class to break.
#[test]
fn validate_accepts_builtin_custom_type_name() {
    let fixture = write_fixture("Ipv4Cidr");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        !diags.iter().any(|d| d.contains("unknown custom type")),
        "validate must accept the built-in `Ipv4Cidr` custom type; got \
         diagnostics: {:#?}",
        diags
    );
}

#[test]
fn validate_routes_registered_bare_identity_through_its_provider_owner() {
    let fixture = write_fixture("TestToken");
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags
            .iter()
            .any(|diagnostic| diagnostic.contains("bare token rejected by its owner")),
        "a registered bare provider type must not remain a fail-open Simple node; got: {diags:#?}"
    );
    assert!(
        !diags
            .iter()
            .any(|diagnostic| diagnostic.contains("Unknown type")),
        "the registered bare type must resolve before validation; got: {diags:#?}"
    );
}

/// `attributes` parameters are the module-callable equivalent of
/// `arguments` for attribute-level lookups, and they share the same
/// silent-accept bug if the post-parse walk is restricted to
/// `arguments` alone. This test pins coverage so a future regression
/// in the walker's parameter set is caught.
#[test]
fn validate_rejects_fake_custom_type_in_attributes() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("inner")).unwrap();

    std::fs::write(
        dir.path().join("inner/main.crn"),
        r#"attributes {
  bad_attr: TotallyMadeUpType = 'placeholder'
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}
"#,
    )
    .unwrap();

    let diags = carina_cli::commands::validate::validate_with_factories(
        &dir.path().join("inner"),
        factories(),
    );

    assert!(
        diags
            .iter()
            .any(|d| d.contains("unknown custom type") && d.contains("TotallyMadeUpType")),
        "validate must reject an unknown bare custom-type name in an \
         `attributes` declaration; got diagnostics: {:#?}",
        diags
    );
}

/// `exports` declarations carry an optional type annotation; an
/// unknown bare custom type there is the same class of silent-accept
/// bug as `arguments` / `attributes`.
#[test]
fn validate_rejects_fake_custom_type_in_exports() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("inner")).unwrap();

    std::fs::write(
        dir.path().join("inner/main.crn"),
        r#"let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  bad_export: TotallyMadeUpType = vpc.vpc_id
}
"#,
    )
    .unwrap();

    let diags = carina_cli::commands::validate::validate_with_factories(
        &dir.path().join("inner"),
        factories(),
    );

    assert!(
        diags
            .iter()
            .any(|d| d.contains("unknown custom type") && d.contains("TotallyMadeUpType")),
        "validate must reject an unknown bare custom-type name in an \
         `exports` declaration; got diagnostics: {:#?}",
        diags
    );
}

/// Standalone-module validate: `carina validate ./my_module/` runs
/// against the module directory directly, with no caller in sight.
/// The root parse used the bootstrap `ProviderContext`
/// (`ProviderCustomTypes::Unloaded`), so the strict parser gate did not fire;
/// the post-parse `validate_argument_custom_types` walk in
/// `validate_and_resolve_errors` with a test `ProviderLoad` is what catches the
/// unknown name here. Without that walk, the bug-headline shape
/// (`arguments { bad_arg: TotallyMadeUpType }` validated standalone)
/// would slip through and the issue would only partially be fixed.
#[test]
fn validate_rejects_fake_custom_type_in_standalone_module() {
    let fixture = write_fixture("TotallyMadeUpType");
    let inner = fixture.path().join("inner");

    let diags = carina_cli::commands::validate::validate_with_factories(&inner, factories());

    assert!(
        diags
            .iter()
            .any(|d| d.contains("unknown custom type") && d.contains("TotallyMadeUpType")),
        "validate must reject an unknown custom-type name even when the \
         module is validated standalone (no caller); got diagnostics: \
         {:#?}",
        diags
    );
}
