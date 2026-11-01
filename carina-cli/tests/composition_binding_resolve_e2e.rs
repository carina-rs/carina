//! End-to-end regression for carina#3246.
//!
//! A managed-resource attribute that references `<module_instance>.<attr>`
//! (where `<module_instance>` is a `LiveComposition` produced by module
//! expansion) must resolve to the concrete managed-sibling literal that
//! backs it. Pre-carina#3248, the plan-path resolver built bindings from
//! the managed slice only, so the composition-rooted ref survived as
//! `ResourceRef` and the differ compared it against the literal already
//! in state — producing a spurious "must be replaced" diff for what is
//! the same value.
//!
//! Post-carina#3248, the resolver consumes a unified `ResolvedBindings`
//! built via `pre_apply`, which lays compositions into the binding map.
//! The composition-rooted ref chains through the composition's attribute map
//! to the managed sibling literal.
//!
//! Per CLAUDE.md "Directory-scoped, never single-file", the fixture
//! mirrors the real infra shape: a caller that imports an outer module
//! which itself imports an inner module. The inner module declares a
//! managed resource and exposes one of its attributes through
//! `attributes { ... }`; the outer module instantiates the inner via
//! `let X = inner { ... }` and references `X.<attr>` from a sibling
//! anonymous resource — exactly the shape of the issue's repro
//! (`envs/registry/dev/bootstrap/`).

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;
use std::collections::HashMap;
use tempfile::TempDir;

struct AwsccTestFactory;

impl ProviderFactory for AwsccTestFactory {
    fn name(&self) -> &str {
        "awscc"
    }
    fn display_name(&self) -> &str {
        "AWSCC (carina#3246 test stub)"
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(NoopProvider)))
        })
    }
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> =
            vec![role_schema(), role_policy_schema()];
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

fn role_schema() -> ResourceSchema {
    ResourceSchema::new("iam.Role")
        .attribute(AttributeSchema::optional("role_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("arn", AttributeType::string()).build())
}

fn role_policy_schema() -> ResourceSchema {
    ResourceSchema::new("iam.RolePolicy")
        .attribute(AttributeSchema::optional("role_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("policy_name", AttributeType::string()).build())
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
            Ok(carina_core::provider::UpdateOutcome::success(
                carina_core::provider::AppliedState::new(id, identifier, HashMap::new())
                    .expect("provider input identifier is non-empty"),
            ))
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
    carina_cli::wiring::LoadedProviderFactory::for_test(vec![
        Box::new(AwsccTestFactory) as Box<dyn ProviderFactory>
    ])
}

fn write_fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");

    std::fs::create_dir(dir.path().join("inner")).unwrap();
    std::fs::create_dir(dir.path().join("outer")).unwrap();
    std::fs::create_dir(dir.path().join("caller")).unwrap();

    std::fs::write(
        dir.path().join("inner/main.crn"),
        r#"arguments {
  role_name: String = 'carina-bootstrap'
}

attributes {
  role_name = role.role_name
}

let role = awscc.iam.Role {
  role_name = role_name
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("outer/main.crn"),
        r#"arguments {
  role_name: String = 'carina-bootstrap'
}

let inner = use { source = '../inner' }

let bootstrap = inner {
  role_name = role_name
}

awscc.iam.RolePolicy {
  role_name   = bootstrap.role_name
  policy_name = "policy-inline"
}
"#,
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
        r#"let outer = use { source = '../outer' }

let bs = outer { }
"#,
    )
    .unwrap();

    dir
}

/// Validate-path smoke: the fixture must validate cleanly. The
/// composition-rooted ref `bootstrap.role_name` is reachable from the
/// anonymous RolePolicy and must not surface as a binding or type
/// error after expansion.
#[test]
fn validate_passes_for_virtual_rooted_ref() {
    let fixture = write_fixture();
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags.is_empty(),
        "validate must produce no errors for the fixture; got: {:#?}",
        diags
    );
}

/// Plan-path regression: a RolePolicy attribute referencing
/// `<module_instance>.role_name` (a composition-rooted ref) must resolve
/// to the concrete literal through the unified `ResolvedBindings`
/// view that `pre_apply` constructs (carina#3246 / carina#3248).
///
/// Pre-fix, the plan-path resolver built bindings from the managed
/// slice only and the composition-rooted ref survived as `ResourceRef`,
/// producing a spurious diff against state's matching literal.
#[test]
fn resolve_refs_for_plan_chains_through_virtual_in_multi_file_fixture() {
    use carina_core::config_loader::load_configuration_with_config;
    use carina_core::parser::ProviderContext;
    use carina_core::resource::{ConcreteValue, DeferredValue, Value};
    use carina_core::schema::SchemaRegistry;

    let fixture = write_fixture();
    let caller = fixture.path().join("caller");

    let provider_context = ProviderContext::default();
    let loaded = load_configuration_with_config(&caller, &provider_context, &SchemaRegistry::new())
        .expect("load should succeed");
    let parsed = loaded.parsed;

    // `load_configuration_with_config` does not run nested module
    // expansion; the CLI command pipeline calls it after validate.
    // Drive it explicitly here so the post-expansion shape (RolePolicy
    // in `resources`, `_virtual.bs.bootstrap` in `compositions`)
    // is in place for the assertion below.
    let parsed = carina_core::validation::deferred_types::with_session_for_test(
        &provider_context,
        |session| {
            carina_core::module_resolver::resolve_modules_with_providerless_config(
                parsed,
                &caller,
                session,
                &SchemaRegistry::new(),
            )
            .expect("module expansion should succeed")
        },
    );
    let parsed = parsed
        .into_expanded()
        .expect("module expansion must substitute every parser placeholder");

    // Sanity: module expansion produced both the RolePolicy resource
    // and the `_virtual.bs.bootstrap` row.
    assert!(
        parsed
            .resources
            .iter()
            .any(|r| r.id.resource_type == "iam.RolePolicy"),
        "RolePolicy must be present after expansion",
    );
    assert!(
        parsed.compositions.iter().any(|v| {
            v.binding.as_ref().map(|binding| binding.as_str()) == Some("bs.bootstrap")
        }),
        "composition `bs.bootstrap` must be present after expansion",
    );

    // Pre-resolve: the RolePolicy carries the composition-rooted ref.
    let policy = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.RolePolicy")
        .expect("RolePolicy");
    match policy.get_attr("role_name") {
        Some(Value::Deferred(DeferredValue::ResourceRef { path })) => {
            assert_eq!(
                path.binding(),
                "bs.bootstrap",
                "expected the composition-rooted ref shape; got: {}",
                path.binding()
            );
        }
        other => panic!("expected ResourceRef pre-resolve, got: {:?}", other),
    }

    // Build the unified pre-apply bindings view per the carina#3248
    // contract and run the plan-path resolver. The fix: `pre_apply`
    // lays compositions into the binding map so the composition-rooted ref
    // chains through to the managed sibling literal.
    let mut resources = parsed.resources.clone();
    let bindings = carina_core::binding_index::ResolvedBindings::pre_apply_live(
        carina_core::binding_index::LivePreApplyInputs {
            managed: &resources,
            compositions: &parsed.compositions,
            managed_current: &HashMap::new(),
            data_source_values: &carina_core::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    );
    carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |session| {
            carina_core::resolver::resolve_refs_for_plan(
                &mut resources,
                &bindings,
                &std::collections::HashSet::new(),
                session,
            )
        },
    )
    .expect("resolve_refs_for_plan should succeed");

    let policy = resources
        .iter()
        .find(|r| r.id.resource_type == "iam.RolePolicy")
        .expect("RolePolicy");
    assert_eq!(
        policy.get_attr("role_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "carina-bootstrap".into()
        ))),
        "expected RolePolicy.role_name to resolve to the concrete literal; got: {:?}",
        policy.get_attr("role_name")
    );
}
