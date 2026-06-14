//! carina#3061 (reopened) — apply-PATH acceptance test.
//!
//! PR #3064's test hand-called `parse_directory → resolve_modules`;
//! that proved the expander in isolation, NOT `carina apply`. This
//! test instead drives the *exact* public sequence `run_apply` runs:
//!
//!   carina-cli/src/commands/apply/mod.rs:477
//!     load_configuration_with_config(path, ...)            (no expand)
//!   carina-cli/src/commands/apply/mod.rs:488
//!     validate_and_resolve_with_config(&mut parsed, ...)   (expands)
//!       -> carina-cli/src/commands/mod.rs:285
//!            module_resolver::resolve_modules_with_config(parsed, ...)
//!   carina-cli/src/commands/apply/mod.rs:999
//!     create_plan(..., &parsed.wait_bindings)
//!
//! So the assertion is: after the apply path's expansion stage,
//! `parsed.wait_bindings` must contain the module-internal `wait`,
//! instance-prefixed (`r.cert_issued`). If it is empty the executor
//! can never form the Distribution -> Wait edge and apply fails at
//! 0.0s with the self-contradicting "add a `wait` block" error.
//!
//! The fixture mirrors carina-rs/infra: caller `let r = registry {
//! use ... }` + module with arguments/attributes + sibling acm.crn
//! (the wait) + cloudfront.crn (nested-map ref to the wait binding).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use carina_cli::commands::validate_and_resolve_errors_with_factories;
use carina_cli::wiring::{PlanPreprocessor, WiringContext};
use carina_core::binding_index::ResolvedBindings;
use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::create_plan;
use carina_core::executor::{ExecutionInput, ExecutionObserver, UnresolvedResource, execute_plan};
use carina_core::parser::ProviderContext;
use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{DataSource, ResourceId, State, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;
use std::sync::Mutex;

// --- Minimal aws provider stub: just enough schema for the fixture's
// aws.acm.Certificate + aws.cloudfront.Distribution to resolve. ---
struct AwsStub;

impl ProviderFactory for AwsStub {
    fn name(&self) -> &str {
        "aws"
    }
    fn display_name(&self) -> &str {
        "AWS (carina#3061 apply-path stub)"
    }
    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }
    fn validate_config(&self, _a: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        _t: &carina_core::schema::TypeIdentity,
        _v: &str,
    ) -> Result<(), String> {
        Ok(())
    }
    fn extract_region(&self, _a: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }
    fn create_provider(
        &self,
        _b: Option<&str>,
        _a: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async {
            Ok(Box::new(NoopProvider {
                cert_publishes_arn: true,
            }) as Box<dyn Provider>)
        })
    }
    fn create_normalizer(
        &self,
        _b: Option<&str>,
        _a: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
    }
    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            ResourceSchema::new("acm.Certificate")
                .attribute(AttributeSchema::new("domain_name", AttributeType::string()))
                .attribute(AttributeSchema::new(
                    "validation_method",
                    AttributeType::string(),
                ))
                .attribute(AttributeSchema::new("status", AttributeType::string()))
                .attribute(AttributeSchema::new(
                    "certificate_arn",
                    AttributeType::string(),
                )),
            // distribution_config's exact shape is irrelevant here —
            // this test asserts wait-binding propagation, not schema
            // validation of the nested map.
            ResourceSchema::new("cloudfront.Distribution").attribute(AttributeSchema::new(
                "distribution_config",
                AttributeType::string(),
            )),
        ]
    }
}

struct NoopProvider {
    /// When false, the ACM cert read returns `status=ISSUED` but
    /// omits `certificate_arn` — modelling a provider whose `read`
    /// satisfies the wait predicate yet never publishes the
    /// attribute the downstream Distribution needs (the suspected
    /// real-infra state-shape root cause for carina#3061).
    cert_publishes_arn: bool,
}
impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "aws"
    }
    fn read(
        &self,
        id: &carina_core::resource::ResourceId,
        _i: Option<&str>,
        _r: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        let publishes_arn = self.cert_publishes_arn;
        Box::pin(async move {
            // Model the ACM cert as already ISSUED so the wait's
            // `until` predicate is satisfied on the first poll. When
            // `publishes_arn` is false the read omits `certificate_arn`
            // — i.e. the wait succeeds but the attribute the
            // downstream Distribution needs is never published. This
            // is the suspected real-infra state-shape root cause.
            if id.resource_type == "acm.Certificate" {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "status".to_string(),
                    Value::Concrete(carina_core::resource::ConcreteValue::String(
                        "ISSUED".to_string(),
                    )),
                );
                if publishes_arn {
                    attrs.insert(
                        "certificate_arn".to_string(),
                        Value::Concrete(carina_core::resource::ConcreteValue::String(
                            "arn:aws:acm:us-east-1:111:certificate/abc".to_string(),
                        )),
                    );
                }
                Ok(
                    carina_core::resource::State::existing(id, attrs)
                        .with_identifier("acm-cert-id"),
                )
            } else {
                Ok(carina_core::resource::State::not_found(id))
            }
        })
    }
    fn read_data_source(
        &self,
        r: &DataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = r.id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn create(
        &self,
        id: &carina_core::resource::ResourceId,
        _r: carina_core::provider::CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn update(
        &self,
        id: &carina_core::resource::ResourceId,
        _i: &str,
        _r: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _i: &str,
        _r: carina_core::provider::DeleteRequest,
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

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wait_apply_module")
        .join(name)
}

#[test]
fn apply_path_propagates_module_wait_binding() {
    let caller = fixture("caller");
    let provider_context = ProviderContext::default();

    // --- exactly run_apply's load+resolve+expand sequence ---
    let loaded = load_configuration_with_config(
        &caller,
        &provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .expect("load_configuration_with_config should succeed");

    let mut parsed = loaded.parsed;
    let base_dir = get_base_dir(&caller);

    let errors = validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        // `skip_resource_validation = true` is a real apply-path mode
        // (run_apply's backend-seed re-load uses it,
        // apply/mod.rs:635). It still runs the *same* module
        // expansion — `resolve_modules_with_config` at
        // commands/mod.rs:285 is gated only on the non-resource
        // checks, not on this flag. Using it here isolates
        // wait-binding propagation from full nested-schema
        // validation (we are not testing the awscc Distribution
        // schema, only that the module's `wait` survives apply
        // expansion).
        true,
        vec![Box::new(AwsStub) as Box<dyn ProviderFactory>],
        HashMap::new(),
    );
    assert!(
        errors.is_empty(),
        "apply-path validate/resolve must succeed for the valid \
         module fixture; got: {:?}",
        errors.iter().map(ToString::to_string).collect::<Vec<_>>()
    );

    // The module's `wait cert` lives two levels in. After the apply
    // path's expansion it MUST reach the caller, instance-prefixed.
    let waits: Vec<(&str, &str)> = parsed
        .wait_bindings
        .iter()
        .map(|w| (w.binding.as_str(), w.target.as_str()))
        .collect();
    assert_eq!(
        parsed.wait_bindings.len(),
        1,
        "apply path must propagate the module-internal `wait` to the \
         caller's wait_bindings; got {waits:?}. Empty here is the \
         carina#3061 apply-path failure: no Effect::Wait is emitted, \
         the Distribution gets no dependency edge, and apply fails at \
         0.0s with the self-contradicting `add a wait block` error."
    );
    let wb = &parsed.wait_bindings[0];
    assert_eq!(wb.binding.as_str(), "r.cert_issued");
    assert_eq!(wb.target.as_str(), "r.cert");

    // And the expanded Distribution's nested ref must point at the
    // prefixed wait binding so the executor can link them.
    let dist = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "cloudfront.Distribution")
        .expect("expanded Distribution must be present");
    let deps = carina_core::deps::get_resource_value_ref_dependencies(
        carina_core::parser::ResourceRef::Resource(dist),
    );
    assert!(
        deps.contains("r.cert_issued"),
        "expanded Distribution must depend on the prefixed wait \
         binding `r.cert_issued`; deps were {deps:?}"
    );

    // --- Diagnostic: the suspected real bug is that the Distribution's
    // `r.cert_issued.certificate_arn` resolves against AccessPath
    // binding `r` (the composition module proxy, which also exposes
    // `certificate_arn` via the module's `attributes` block) instead
    // of `r.cert_issued` (the wait binding). Dump the actual
    // AccessPath and whether a composition `r` shadows it.
    use carina_core::resource::{ConcreteValue, DeferredValue, Value};
    fn find_ref<'a>(v: &'a Value, acc: &mut Vec<&'a carina_core::resource::AccessPath>) {
        match v {
            Value::Deferred(DeferredValue::ResourceRef { path }) => acc.push(path),
            Value::Concrete(ConcreteValue::Map(m)) => m.values().for_each(|x| find_ref(x, acc)),
            Value::Concrete(ConcreteValue::List(l)) => l.iter().for_each(|x| find_ref(x, acc)),
            _ => {}
        }
    }
    let dc = dist
        .get_attr("distribution_config")
        .expect("distribution_config present");
    let mut refs = Vec::new();
    find_ref(dc, &mut refs);
    let paths: Vec<String> = refs
        .iter()
        .map(|p| {
            format!(
                "binding={:?} attribute={:?} segments={:?}",
                p.binding(),
                p.attribute(),
                p.segments()
            )
        })
        .collect();
    let composition_r = parsed
        .compositions
        .iter()
        .any(|r| r.binding.as_deref() == Some("r"));
    // This assertion encodes the hypothesis. If it FAILS showing
    // binding="r", we've found the apply-path root cause: the nested
    // ref binds to the composition module proxy, not the wait binding.
    assert!(
        refs.iter().any(|p| p.binding() == "r.cert_issued"),
        "Distribution's distribution_config ref must bind to the wait \
         binding `r.cert_issued`, NOT the composition module proxy `r`. \
         Found refs: {paths:?}. Composition `r` present: {composition_r}"
    );
}

struct CollectingObserver {
    failures: Mutex<Vec<String>>,
}
impl ExecutionObserver for CollectingObserver {
    fn on_event(&self, e: &carina_core::executor::ExecutionEvent) {
        if let carina_core::executor::ExecutionEvent::EffectFailed { error, .. } = e {
            self.failures.lock().unwrap().push(error.to_string());
        }
    }
}

/// Full apply transform chain (apply/mod.rs:488 → 999 → execute_plan):
/// after expansion, run sort → resolve_refs → canonicalize →
/// PlanPreprocessor::prepare → create_plan → execute_plan, exactly as
/// `run_apply`. This is the path #3064's test never exercised. The
/// real symptom is the Distribution failing at 0.0s with the
/// self-contradicting "add a `wait` block" error.
/// Drive the full apply transform chain (apply/mod.rs:488 → 999 →
/// execute_plan) — sort → resolve_refs → canonicalize →
/// PlanPreprocessor::prepare → create_plan → execute_plan, exactly as
/// `run_apply`. `cert_publishes_arn` controls whether the cert's
/// `read` (which the wait polls) includes `certificate_arn`. Returns
/// (failure_count, skip_count, failure messages).
async fn run_apply_chain(cert_publishes_arn: bool) -> (usize, usize, Vec<String>) {
    let caller = fixture("caller");
    let provider_context = ProviderContext::default();

    let loaded = load_configuration_with_config(
        &caller,
        &provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .expect("load should succeed");
    let mut parsed = loaded.parsed;
    let base_dir = get_base_dir(&caller);

    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AwsStub)];
    let errs = validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        true,
        vec![Box::new(AwsStub) as Box<dyn ProviderFactory>],
        HashMap::new(),
    );
    assert!(errs.is_empty(), "expand stage must succeed: {errs:?}");

    let ctx = WiringContext::new(factories);
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources).expect("topo sort");

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

    let mut resources_for_plan = sorted_resources.clone();
    let wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    let bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
        carina_core::binding_index::PreApplyInputs {
            managed: &resources_for_plan.clone(),
            compositions: &parsed.compositions,
            data_sources: &[],
            current_states: &current_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        },
    );
    carina_core::resolver::resolve_refs_with_state_and_remote(&mut resources_for_plan, &bindings)
        .expect("resolve_refs");

    carina_core::value::canonicalize_resources_with_schemas(&mut resources_for_plan, ctx.schemas());
    carina_core::value::canonicalize_states_with_schemas(&mut current_states, ctx.schemas());

    let provider = NoopProvider { cert_publishes_arn };
    let mut wait_bindings = parsed.wait_bindings.clone();
    let preprocessor = PlanPreprocessor::new(&NoopNormalizer, &ctx);
    preprocessor
        .prepare(
            &mut resources_for_plan,
            &mut current_states,
            &parsed.providers,
            &parsed.data_sources,
            &mut wait_bindings,
        )
        .await;

    let plan = create_plan(
        &resources_for_plan,
        &parsed.data_sources,
        &current_states,
        &HashMap::new(),
        ctx.schemas(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &wait_bindings,
    );

    let has_wait = plan.effects().iter().any(|e| {
        matches!(e, carina_core::effect::Effect::Wait { binding, .. } if binding == "r.cert_issued")
    });
    assert!(
        has_wait,
        "create_plan must emit Effect::Wait for `r.cert_issued`; effects: {:?}",
        plan.effects()
    );

    let unresolved_resources: HashMap<ResourceId, _> = sorted_resources
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                UnresolvedResource::from_pre_resolve(r.clone()),
            )
        })
        .collect();
    let observer = CollectingObserver {
        failures: Mutex::new(Vec::new()),
    };
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &unresolved_resources,
        compositions: &parsed.compositions,
        bindings: ResolvedBindings::default(),
        current_states,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        // carina#3063 (#3069): the executor re-applies the full
        // plan-time normalization pipeline on the apply path. Pass the
        // same factories/schemas the apply path threads through.
        factories: ctx.factories(),
        schemas: ctx.schemas(),
        parallelism: carina_core::executor::TEST_UNCAPPED,
    };
    let result = execute_plan(&provider, input, &observer).await;
    let failures = observer.failures.lock().unwrap().clone();
    (result.failure_count, result.skip_count, failures)
}

/// Positive control: when the cert read publishes `certificate_arn`,
/// the whole apply chain works — the Distribution waits and succeeds.
#[tokio::test]
async fn apply_chain_distribution_waits_for_module_wait_binding() {
    let (failure_count, skip_count, failures) = run_apply_chain(true).await;
    assert_eq!(
        failure_count, 0,
        "Distribution must wait for r.cert_issued, not fail at dispatch. \
         Failures: {failures:?}"
    );
    assert_eq!(
        skip_count, 0,
        "no effect should be skipped. Failures: {failures:?}"
    );
}

/// Root-cause hypothesis (carina#3061): the real ACM provider's `read`
/// satisfies the wait predicate (`status == ISSUED`) but does NOT
/// publish `certificate_arn` in the polled state. The wait then
/// succeeds, `record_applied` registers `r.cert_issued` with a
/// `certificate_arn`-less attribute map, and the Distribution's
/// `r.cert_issued.certificate_arn` cannot resolve — producing the
/// EXACT real-infra error. This reproduces that offline.
#[tokio::test]
async fn apply_chain_repros_when_wait_state_lacks_published_attr() {
    let (failure_count, _skip, failures) = run_apply_chain(false).await;
    assert!(
        failure_count >= 1,
        "expected the Distribution to fail when the wait-captured \
         cert state lacks `certificate_arn`"
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("certificate_arn") && f.contains("has not been published")),
        "must reproduce the exact real-infra error \
         (`...certificate_arn which has not been published...`); \
         got: {failures:?}"
    );
}
