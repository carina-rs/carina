//! End-to-end regression for carina#3246.
//!
//! A managed-resource attribute that references `<module_instance>.<attr>`
//! (where `<module_instance>` is a `Composition` produced by module
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
use carina_core::resource::{DataSource, Value};
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
    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }
    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        _type_name: &carina_core::schema::TypeIdentity,
        _value: &str,
    ) -> Result<(), String> {
        Ok(())
    }
    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }
    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(NoopProvider) as Box<dyn Provider>) })
    }
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
    }
    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![role_schema(), role_policy_schema()]
    }
}

fn role_schema() -> ResourceSchema {
    ResourceSchema::new("iam.Role")
        .attribute(AttributeSchema::new("role_name", AttributeType::string()))
        .attribute(AttributeSchema::new("arn", AttributeType::string()))
}

fn role_policy_schema() -> ResourceSchema {
    ResourceSchema::new("iam.RolePolicy")
        .attribute(AttributeSchema::new("role_name", AttributeType::string()))
        .attribute(AttributeSchema::new("policy_name", AttributeType::string()))
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "awscc"
    }
    fn read(
        &self,
        id: &carina_core::resource::ResourceId,
        _identifier: Option<&str>,
        _request: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::not_found(id)) })
    }
    fn read_data_source(
        &self,
        resource: &DataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn create(
        &self,
        id: &carina_core::resource::ResourceId,
        _request: carina_core::provider::CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn update(
        &self,
        id: &carina_core::resource::ResourceId,
        _identifier: &str,
        _request: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &str,
        _request: carina_core::provider::DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn required_permissions(
        &self,
        _id: &carina_core::resource::ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> Vec<String> {
        Vec::new()
    }
}

fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(AwsccTestFactory) as Box<dyn ProviderFactory>]
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
    let mut parsed = loaded.parsed;

    // `load_configuration_with_config` does not run nested module
    // expansion; the CLI command pipeline calls it after validate.
    // Drive it explicitly here so the post-expansion shape (RolePolicy
    // in `resources`, `_virtual.bs.bootstrap` in `compositions`)
    // is in place for the assertion below.
    carina_core::module_resolver::resolve_modules_with_config(
        &mut parsed,
        &caller,
        &provider_context,
    )
    .expect("module expansion should succeed");

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
        parsed
            .compositions
            .iter()
            .any(|v| v.binding.as_deref() == Some("bs.bootstrap")),
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
    let bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
        carina_core::binding_index::PreApplyInputs {
            managed: &resources,
            compositions: &parsed.compositions,
            data_sources: &parsed.data_sources,
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    );
    carina_core::resolver::resolve_refs_for_plan(
        &mut resources,
        &bindings,
        &std::collections::HashSet::new(),
    )
    .expect("resolve_refs_for_plan should succeed");

    let policy = resources
        .iter()
        .find(|r| r.id.resource_type == "iam.RolePolicy")
        .expect("RolePolicy");
    assert_eq!(
        policy.get_attr("role_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "carina-bootstrap".to_string()
        ))),
        "expected RolePolicy.role_name to resolve to the concrete literal; got: {:?}",
        policy.get_attr("role_name")
    );
}
