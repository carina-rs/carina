//! Plan display snapshot tests.
//!
//! Each test loads a .crn fixture (and optionally a state file), builds a plan
//! using the same logic as `--refresh=false`, formats the plan output, strips
//! ANSI color codes, and asserts the result against an `insta` snapshot.

use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::config_loader::load_configuration;
use carina_core::effect::Effect;
use carina_core::module_resolver::{
    ConstraintOrigin, ModuleConstraintCall, ModuleConstraintDiagnostic, ModuleConstraintKind,
};
use carina_core::plan::{Plan, PlanError, PlanErrorKind};
use carina_core::resource::{
    Current, DataSource, ResolvedDataSource, ResolvedResource, Resource, ResourceId,
};
use carina_core::schema::SchemaRegistry;

use crate::DetailLevel;
use crate::display::{
    format_destroy_plan_with_delete_instances, format_plan_with_delete_instances,
};
use crate::fixture_plan::{
    FixturePlan, build_fixture_plan_change_summary, build_plan_from_fixture_name,
    provider_router_for_plan,
};
use crate::moved_hint::MovedBlockHint;
use crate::plan_summary::PlanChangeSummary;

#[allow(clippy::too_many_arguments)]
fn format_plan<D: crate::display::HistoricalDisplaySource + ?Sized>(
    color_mode: crate::output_style::ColorMode,
    plan: &Plan,
    detail: DetailLevel,
    historical_display: &D,
    schemas: Option<&SchemaRegistry>,
    moved_origins: &HashMap<ResourceId, ResourceId>,
    export_changes: &[crate::commands::plan::ExportChange],
    deferred_for_expressions: &[carina_core::parser::DeferredForExpression],
    prev_explicit: Option<&HashMap<ResourceId, carina_core::explicit::ExplicitFields>>,
    expansion_trace: Option<&carina_core::resource::ExpansionTrace>,
) -> String {
    let deferred_for_expressions = crate::plan_summary::resolved_deferred_for_for_test(
        deferred_for_expressions.iter().cloned(),
    );
    let summary = PlanChangeSummary::new(plan, export_changes, &deferred_for_expressions, &[]);
    let provider = provider_router_for_plan(&crate::test_support::TestCommandRuntime::new(), plan)
        .expect("fixture mock inputs must be valid");
    format_plan_with_delete_instances(
        color_mode,
        plan,
        &summary,
        detail,
        historical_display,
        schemas,
        moved_origins,
        prev_explicit,
        expansion_trace,
        &provider,
        carina_core::detail_rows::DetailComparisons::Planning(
            &carina_core::references::prepare_empty_for_tests(),
        ),
    )
    .expect("snapshot plan must have an initialized provider route")
}

fn format_destroy_plan<D: crate::display::HistoricalDisplaySource + ?Sized>(
    color_mode: crate::output_style::ColorMode,
    plan: &Plan,
    detail: DetailLevel,
    historical_display: &D,
) -> String {
    let provider = provider_router_for_plan(&crate::test_support::TestCommandRuntime::new(), plan)
        .expect("fixture mock inputs must be valid");
    format_destroy_plan_with_delete_instances(
        color_mode,
        plan,
        detail,
        historical_display,
        &provider,
    )
    .expect("snapshot destroy plan must have an initialized provider route")
}

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
    ResolvedDataSource::new(resource)
}

/// Strip ANSI escape codes from a string for snapshot readability.
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

fn nested_write_only_listener_block<'a>(output: &'a str, name: &str) -> &'a str {
    const LISTENER_PREFIX: &str = "awscc.elasticloadbalancingv2.Listener ";
    let heading = format!("{LISTENER_PREFIX}{name}");
    let start = output
        .match_indices(&heading)
        .find_map(|(start, _)| {
            let suffix = &output[start + heading.len()..];
            (suffix.starts_with(' ') || suffix.starts_with('\n')).then_some(start)
        })
        .unwrap_or_else(|| panic!("missing listener {name} in plan output:\n{output}"));
    let content_start = start + heading.len();
    let remaining = &output[content_start..];
    let end = remaining
        .find(LISTENER_PREFIX)
        .or_else(|| remaining.find("\nPlan:"))
        .unwrap_or(remaining.len());

    &output[start..content_start + end]
}

/// Build a plan from a .crn fixture and optional state file, mimicking `--refresh=false`.
fn build_plan_from_fixture(
    color_mode: crate::output_style::ColorMode,
    fixture_dir: &str,
) -> (
    carina_core::plan::Plan,
    SchemaRegistry,
    HashMap<ResourceId, ResourceId>,
) {
    let runtime = crate::test_support::TestCommandRuntime::new().with_color_mode(color_mode);
    let fp = build_plan_from_fixture_name(&runtime, fixture_dir)
        .expect("fixture mock inputs must be valid");
    (fp.plan, fp.schemas, fp.moved_origins)
}

#[allow(clippy::type_complexity)]
fn build_plan_and_states_from_fixture(
    color_mode: crate::output_style::ColorMode,
    fixture_dir: &str,
) -> (
    carina_core::plan::Plan,
    HashMap<ResourceId, Current>,
    SchemaRegistry,
    HashMap<ResourceId, ResourceId>,
) {
    let runtime = crate::test_support::TestCommandRuntime::new().with_color_mode(color_mode);
    let fp = build_plan_from_fixture_name(&runtime, fixture_dir)
        .expect("fixture mock inputs must be valid");
    (fp.plan, fp.managed_current, fp.schemas, fp.moved_origins)
}

fn delete_attributes_for_plan(
    plan: &Plan,
    current_states: &HashMap<ResourceId, Current>,
) -> carina_core::detail_rows::HistoricalDisplayAttributes {
    crate::commands::plan::collect_historical_display_attributes(plan, current_states, None)
}

/// Plan-display gate for `directives.depends_on` (#2823). The bucket
/// declares an explicit ordering edge to `role` with no value
/// reference; the snapshot pins how the plan tree renders that edge.
#[test]
fn snapshot_depends_on() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "depends_on");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_ignore_changes_readable_drift() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "ignore_changes_readable")
        .expect("fixture mock inputs must be valid");
    assert_eq!(fp.plan.ignored_change_summaries().len(), 2);
    assert!(
        fp.plan
            .ignored_change_summaries()
            .iter()
            .all(|summary| summary.count() == 1)
    );

    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    assert_eq!(output.matches("# 1 ignored attribute(s) differ").count(), 2);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_postfix_access() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "postfix_access");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));

    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_integer_arithmetic_is_structural() {
    let expression = carina_core::integer_arithmetic::evaluate_integer_values(
        carina_core::resource::Value::<carina_core::resource::LoopSlot>::Deferred(
            carina_core::resource::DeferredValue::Placeholder(
                carina_core::resource::LoopSlot::Value,
            ),
        ),
        carina_core::integer_arithmetic::IntegerOp::Divide,
        carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::Int(60)),
        carina_core::integer_arithmetic::IntegerOperatorSpan::synthetic(),
    )
    .expect("deferred Int arithmetic must be structurally valid");
    let rendered = carina_core::plan_tree::render_deferred_for_template_value(
        &expression,
        "source",
        "applies",
    );

    insta::assert_snapshot!(rendered.plain(), @"(known after source applies) / 60");
}

#[test]
fn snapshot_iam_preflight_warning_appears_after_plan_summary() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "depends_on");
    let mut output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    let warning = crate::commands::iam_preflight::format_warnings(
        &crate::commands::iam_preflight::IamPreflightResult::Checked(
            crate::commands::iam_preflight::IamPreflightReport {
                actor_arn: "arn:aws:sts::123456789012:assumed-role/deploy/session".to_string(),
                method: crate::commands::iam_preflight::IamCheckMethod::SimulatePrincipalPolicy,
                source_providers: vec!["aws".to_string(), "awscc".to_string()],
                missing_by_effect: vec![crate::commands::iam_preflight::MissingEffectActions {
                    effect: crate::commands::iam_preflight::EffectAddress {
                        resource: "awscc.elasticloadbalancingv2.LoadBalancer registry_publish.alb"
                            .to_string(),
                        op: carina_core::effect::PlanOp::Create,
                    },
                    missing_actions: vec!["ec2:DescribeInternetGateways".to_string()],
                }],
            },
        ),
    )
    .expect("warning should render");
    output.push_str(&strip_ansi(&warning));
    output.push('\n');

    let execution_plan = output.find("Execution Plan:").expect("plan header");
    let summary = output.find("Plan: ").expect("plan summary");
    let iam = output
        .find("IAM preflight findings")
        .expect("iam warning block");

    assert!(
        execution_plan < summary,
        "plan summary should follow header"
    );
    assert!(summary < iam, "IAM warning should follow plan summary");
    insta::assert_snapshot!(output);
}

/// Plan-display gate for the `wait` construct (carina#2825). The
/// fixture wires ACM Certificate → Route53 validation record → wait
/// (blocking on `cert.status == ISSUED`). The snapshot pins:
/// - the `> cert_issued (until cert.status == aws.acm.Certificate.Status.Issued)`
///   line format (one-character marker `>`, predicate surface form
///   echoed verbatim from the user source),
/// - that wait effects are placed in the tree as children of their
///   target (`cert`) — the tree shape is wired in
///   `plan_tree::build_dependency_graph`.
#[test]
fn snapshot_wait_cert() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "wait_cert");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_all_create() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "all_create");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_resolved_value_constraint_multifile() {
    let runtime = crate::test_support::TestCommandRuntime::new();

    let fp = build_plan_from_fixture_name(&runtime, "resolved_value_constraint_multifile")
        .expect("fixture mock inputs must be valid");
    assert!(
        !fp.plan.errors().is_empty(),
        "the fixture planner must run the resolved-value constraint gate"
    );
    let output = fp
        .plan
        .errors()
        .iter()
        .map(|error| {
            error
                .display_with_project_dir(std::path::Path::new("."))
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_module_constraint_plan_error() {
    let error = PlanError::new(
        ResourceId::with_identity("_virtual", "c"),
        PlanErrorKind::ModuleConstraint(ModuleConstraintDiagnostic {
            module: "mod".to_string(),
            instance: "c".to_string(),
            origin: ConstraintOrigin::Saved,
            call: ModuleConstraintCall::Named("c".to_string()),
            kind: ModuleConstraintKind::ArgumentValidation {
                argument: "name".to_string(),
            },
            arguments: vec!["name".to_string()],
            message: "too long".to_string(),
            actuals: vec![("name".to_string(), "\"abcd\"".to_string())],
            detail: None,
        }),
    );

    let rendered = error
        .display_with_project_dir(std::path::Path::new("."))
        .to_string();
    assert_eq!(
        rendered,
        "module 'mod' (call 'c'): argument 'name': too long (got \"abcd\")"
    );
    insta::assert_snapshot!(rendered);
}

#[test]
fn non_module_anonymous_dependency_uses_schema_derived_identity() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, _schemas, _moved) = build_plan_from_fixture(color_mode, "all_create");
    let route_table = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id().resource_type == "ec2.RouteTable")
        .expect("all_create must contain the anonymous route table");

    assert_eq!(
        route_table
            .resource_id()
            .identity_str()
            .expect("resolved identity"),
        "awscc_ec2_route_table_634bd335",
        "the route table identity must be derived from its schema's create-only vpc_id"
    );
}

/// carina#2191 Phase 3b-2b acceptance: a directory with one default
/// instance (`provider awscc { ... }`) and one named instance
/// (`let us = provider awscc { ... }`), plus two resources where only
/// the second carries `directives { provider = us }`. The snapshot
/// pins the rendered plan output for the two-resource shape; the
/// companion `multi_instance_create_propagates_provider_instance`
/// test asserts the binding is carried on `ResourceId.provider_instance`
/// so apply-time routing has it available (plan display is intentionally
/// silent about the instance — that surface is owned by a later phase).
#[test]
fn snapshot_multi_instance_create() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "multi_instance_create");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// Acceptance for the parse → plan flow on the multi-instance fixture:
/// the `us_bucket` resource — the only one carrying
/// `directives { provider = us }` — must reach the plan with
/// `ResourceId.provider_instance == Some("us")`. The other bucket and
/// the kind's default instance must remain `None`. This is what
/// `ProviderRouter::get_provider_or_error` keys on at apply time.
#[test]
fn multi_instance_create_propagates_provider_instance() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, _schemas, _moved) = build_plan_from_fixture(color_mode, "multi_instance_create");

    let mut by_name: HashMap<String, Option<String>> = HashMap::new();
    for effect in plan.effects() {
        let id = effect.resource_id();
        by_name.insert(
            id.identity_str().expect("resolved identity").to_string(),
            id.provider_instance.clone(),
        );
    }

    assert_eq!(
        by_name.get("tokyo_bucket"),
        Some(&None),
        "tokyo_bucket has no directives — must route to the kind default (provider_instance = None)"
    );
    assert_eq!(
        by_name.get("us_bucket"),
        Some(&Some("us".to_string())),
        "us_bucket has directives {{ provider = us }} — must carry the binding through to the plan"
    );
}

/// carina#3040 regression: when a named provider instance routes a
/// resource that lives *inside an expanded module* (the real-infra
/// shape — `usecases/registry/acm.crn` consumed via `use { source =
/// ... }`, with `directives { provider = us }` on the module-internal
/// resource), the directive must survive module expansion onto
/// `ResourceId.provider_instance`. Before the fix the create call
/// routed to the kind default (ap-northeast-1) even though the state
/// row recorded `provider_instance: "us"`, leaving an ACM cert in the
/// wrong region. The companion `multi_instance_create_propagates_
/// provider_instance` test covers the non-module shape; this one
/// covers the module-expanded shape specifically.
#[test]
fn module_routed_instance_propagates_provider_instance() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, _schemas, _moved) = build_plan_from_fixture(color_mode, "multi_instance_module");

    let routed: Vec<(String, Option<String>)> = plan
        .effects()
        .iter()
        .map(|e| {
            let id = e.resource_id();
            (
                id.identity_str().expect("resolved identity").to_string(),
                id.provider_instance.clone(),
            )
        })
        .collect();

    let cert = routed
        .iter()
        .find(|(name, _)| name.ends_with("cert"))
        .unwrap_or_else(|| {
            panic!("expected a module-expanded `cert` resource in the plan, got {routed:?}")
        });

    assert_eq!(
        cert.1,
        Some("us".to_string()),
        "module-internal resource carries `directives {{ provider = us }}` — it must reach \
         the plan with ResourceId.provider_instance == Some(\"us\") so apply-time \
         ProviderRouter routing sends the create call to the `us` instance, not the \
         kind default. Got {routed:?}"
    );
}

/// Locks in the no-trailing-dot regression from #2516. The hash-with-
/// instance-prefix path is covered by the unit tests in
/// `carina-core/src/identifier/tests.rs`; fixture mode skips the hash
/// step (no schemas), so the rendered name is empty rather than
/// `bootstrap.<hash>` — the value of this snapshot is asserting that
/// the anonymous resource is *not* rendered as `bootstrap.`.
#[test]
fn snapshot_module_anonymous_resource() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "module_anonymous_resource");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_policy_pretty() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "policy_pretty");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_policy_pretty_nested() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "policy_pretty_nested");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// carina#3356: a `List<Struct>` attribute on a resource that is a
/// *dependent child* in the plan tree. Its attribute rows carry the `│`
/// tree gutter, and every physical row of the multi-line list value
/// (each `* ...` element and its nested lines) must inherit that gutter
/// and indent — not just the first row. Pre-fix the list body floated
/// outside the tree at a shallower indent with no `│` until rendering
/// snapped back to the gutter on the next scalar attribute. This is the
/// CLI analogue of #2523 (which was TUI-only) for the list-expansion
/// path.
#[test]
fn snapshot_list_struct_child_gutter() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "list_struct_child_gutter");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_policy_pretty_dynamic_key_list() {
    let color_mode = crate::output_style::ColorMode::Plain;
    // #2528 acceptance: an IAM trust-policy `condition.<op>.<context-key>:
    // [list]` shape — a multi-element list-of-strings nested under a
    // dynamic-key Map — must break across lines all the way down, the
    // same way `statement[].action` does. Pre-fix the deepest list
    // collapsed to one line because the inline-vs-vertical decision did
    // not bubble down to nested Map values.
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "policy_pretty_dynamic_key_list");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_pretty_long_string_list() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "pretty_long_string_list");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_pretty_short_string_list() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "pretty_short_string_list");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, _schemas, _moved) = build_plan_from_fixture(color_mode, "no_changes");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_state_only_directive_change() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "state_only_directive_change")
        .expect("fixture mock inputs must be valid");
    assert!(
        fp.plan.effects().is_empty(),
        "the fixture must contain no provider-visible effects"
    );
    let summary = build_fixture_plan_change_summary(&fp);
    let provider =
        provider_router_for_plan(&runtime, &fp.plan).expect("fixture mock inputs must be valid");
    let historical_display =
        carina_core::detail_rows::HistoricalDisplayAttributes::for_plan(&fp.plan);
    let output = strip_ansi(
        &format_plan_with_delete_instances(
            color_mode,
            &fp.plan,
            &summary,
            DetailLevel::Full,
            &historical_display,
            Some(&fp.schemas),
            &fp.moved_origins,
            Some(&fp.prev_explicit),
            Some(&fp.expansion_trace),
            &provider,
            fp.references.for_display(),
        )
        .expect("state-only fixture plan must render"),
    );

    insta::assert_snapshot!(output);
}

/// carina#3280: a state row persisted with `explicit.children: {}` (the
/// legacy for-loop-child corruption shape) must not surface every
/// attribute as a spurious diff. Pre-fix `project_attributes` filtered
/// every key out (empty children → empty projected_current), making
/// every per-attribute equality check return `false` and rendering a
/// `~ Change` (or `forces_replacement` Replace, depending on schema)
/// row for each attribute. Post-fix the empty top-level `Struct` is
/// treated as "no authoring record" — projection passes attrs through
/// unchanged — and the desired-matches-state shape produces
/// `No changes. Infrastructure is up-to-date.`
#[test]
fn snapshot_empty_explicit_children_no_changes() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "empty_explicit_children_no_changes")
        .expect("fixture mock inputs must be valid");
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        Some(&fp.prev_explicit),
        None,
    ));
    assert!(
        output.contains("No changes"),
        "carina#3280: fixture with empty `explicit.children` and matching desired \
         must render `No changes.`; pre-fix it surfaced every attribute as a \
         spurious diff. Got:\n{output}"
    );
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_mixed_operations() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "mixed_operations");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_replace_create_only() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "replace_create_only");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_create_only_rekey() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "anonymous_create_only_rekey");
    assert_eq!(
        plan.summary().replace,
        1,
        "partial create-only match must remain a Replace"
    );
    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Move { .. })),
        "anonymous state re-key must emit a Move"
    );
    assert_eq!(moved_origins.len(), 1);

    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    assert!(output.contains("(moved from: test_test_widget_11111111)"));
    insta::assert_snapshot!(output);
}

#[test]
fn anonymous_old_identity_name_override_is_a_pure_move() {
    let runtime = crate::test_support::TestCommandRuntime::new();

    let fixture = build_plan_from_fixture_name(&runtime, "anonymous_old_identity_name_override")
        .expect("fixture mock inputs must be valid");
    let summary = fixture.plan.summary();

    assert_eq!(summary.create, 0);
    assert_eq!(summary.update, 0);
    assert_eq!(summary.replace, 0);
    assert_eq!(summary.delete, 0);
    assert_eq!(summary.moved, 1);
    assert_eq!(fixture.plan.effects().len(), 1);
    assert!(matches!(fixture.plan.effects()[0], Effect::Move { .. }));
}

#[test]
fn snapshot_anonymous_old_identity_update() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "anonymous_old_identity_update");
    assert_eq!(plan.summary().update, 1);
    assert_eq!(plan.summary().moved, 1);

    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    assert!(output.contains("~ awscc.ec2.Vpc awscc_ec2_vpc_0bc2a2bb"));
    assert!(output.contains("(moved from: ec2_vpc_0bc2a2bb)"));
    assert!(output.contains("1 to change, 0 to destroy, 1 to move"));
    insta::assert_snapshot!(output);
}

fn assert_anonymous_loop_fixture(
    color_mode: crate::output_style::ColorMode,
    fixture_name: &str,
    expected_create: usize,
    expected_delete: usize,
    expected_moved: usize,
) -> String {
    let runtime = crate::test_support::TestCommandRuntime::new().with_color_mode(color_mode);
    let fixture = build_plan_from_fixture_name(&runtime, fixture_name)
        .expect("fixture mock inputs must be valid");
    let summary = fixture.plan.summary();

    assert_eq!(
        summary.create, expected_create,
        "{fixture_name}: create count"
    );
    assert_eq!(summary.update, 0, "{fixture_name}: update count");
    assert_eq!(
        summary.replace, 0,
        "{fixture_name}: replacement means a legacy row cross-moved"
    );
    assert_eq!(
        summary.delete, expected_delete,
        "{fixture_name}: delete count"
    );
    assert_eq!(summary.moved, expected_moved, "{fixture_name}: move count");
    assert_eq!(
        fixture.moved_origins.len(),
        expected_moved,
        "{fixture_name}: every move must retain its source address",
    );

    strip_ansi(&format_plan(
        color_mode,
        &fixture.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fixture.schemas),
        &fixture.moved_origins,
        &[],
        &[],
        None,
        None,
    ))
}

#[test]
fn snapshot_anonymous_loop_insert_before() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_insert_before", 1, 0, 2);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_origin_labels() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_insert_before", 1, 0, 2);
    let origin_lines = output
        .lines()
        .filter(|line| line.contains("(for ["))
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(origin_lines, @r###"
      -> awscc.ec2.Vpc awscc_ec2_vpc_0bc2a2bb (moved from: _for0[0])  (for [0] at main.crn:11)
      -> awscc.ec2.Vpc awscc_ec2_vpc_23be212c (moved from: _for0[1])  (for [1] at main.crn:11)
      + awscc.ec2.Vpc awscc_ec2_vpc_2620ba6c  (for [0] at main.crn:5)
    "###);
}

#[test]
fn snapshot_anonymous_loop_reorder() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_reorder", 0, 0, 3);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_delete_earlier() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output =
        assert_anonymous_loop_fixture(color_mode, "anonymous_loop_delete_earlier", 0, 2, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_sibling_files() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_sibling_files", 1, 0, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_same_tail() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_same_tail", 1, 0, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_if_legacy() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_if_legacy", 0, 0, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_deferred_map() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_deferred_map", 0, 0, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_module_call() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_module_call", 0, 0, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_claimed_legacy() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output =
        assert_anonymous_loop_fixture(color_mode, "anonymous_loop_claimed_legacy", 1, 0, 1);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_anonymous_loop_new_id_exists() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let output = assert_anonymous_loop_fixture(color_mode, "anonymous_loop_new_id_exists", 0, 1, 0);
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_replace_with_non_forcing_diffs() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "replace_with_non_forcing_diffs");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_delete_orphan() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture(color_mode, "delete_orphan");
    let delete_attributes = delete_attributes_for_plan(&plan, &current_states);
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deposed_delete() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "deposed_delete")
        .expect("fixture mock inputs must be valid");
    let delete_attributes = crate::fixture_plan::delete_instance_attributes_from_plan(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&fp.schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn fixture_removed_block_suppresses_prior_missing_identifier_error() {
    let runtime = crate::test_support::TestCommandRuntime::new();

    let fp = build_plan_from_fixture_name(&runtime, "removed_missing_identifier")
        .expect("fixture mock inputs must be valid");

    assert!(
        fp.plan.errors().is_empty(),
        "fixture planning must apply removed-block recovery after the differ: {:?}",
        fp.plan.errors()
    );
    assert!(matches!(fp.plan.effects(), [Effect::Remove { .. }]));
    assert_eq!(
        fp.plan.abandoned_deposed_generations().count(),
        1,
        "fixture planning must also suppress the later deposed identifier error"
    );
}

#[test]
fn snapshot_deposed_delete_update_dependency() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "deposed_delete_update_dependency")
        .expect("fixture mock inputs must be valid");
    let delete_attributes = crate::fixture_plan::delete_instance_attributes_from_plan(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&fp.schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        output.contains("  ~ awscc.ec2.Vpc main\n")
            && output.contains("        └─ ~ awscc.ec2.Subnet subnet"),
        "dependent subnet must render under the current main update, not under the deposed delete:\n{output}"
    );
    insta::assert_snapshot!(output);
}

/// A deleted orphan whose state carries a list-of-maps attribute
/// (`domain_validation_options`) must render that attribute vertically
/// (multi-line), not on one long unreadable line. Regression for the
/// Delete path being asymmetric with the Create path in
/// `build_detail_rows` / `build_delete_rows`.
#[test]
fn snapshot_delete_orphan_list_of_maps() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture(color_mode, "delete_orphan_list_of_maps");
    let delete_attributes = delete_attributes_for_plan(&plan, &current_states);
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_compact() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, _schemas, _moved) = build_plan_from_fixture(color_mode, "compact");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_map_key_diff() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "map_key_diff");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// #2936: a Map<String, String> attribute that goes from absent in
/// state to a multi-key value must render as per-key `+ key: "value"`
/// lines, not as a single inline `tags: (none) → {...}` line.
#[test]
fn snapshot_map_added_from_none() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "map_added_from_none");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// #2939: a Map<String, String> attribute that goes from a multi-key
/// value in state to absent in desired must render as per-key
/// `- key: "value"` lines, not as a single inline
/// `tags: {...} → (removed)` line. Mirrors `snapshot_map_added_from_none`
/// for the removal direction.
#[test]
fn snapshot_map_attribute_removed() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "map_attribute_removed");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_enum_display() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "enum_display");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_string_dot_suffix() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "string_dot_suffix");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert_eq!(
        output.matches("name: 'A.json'").count(),
        2,
        "literal and evaluated string values must both retain their dotted suffix:\n{output}"
    );
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes_enum() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, _schemas, _moved) = build_plan_from_fixture(color_mode, "no_changes_enum");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn dynamic_enum_az_state_api_spelling_has_no_diff_in_cli_plan_path() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "dynamic_enum_az_no_diff");
    assert!(
        plan.effects().is_empty(),
        "API-spelled saved state must compare equal to DSL-spelled desired state through the CLI plan path:\n{}",
        strip_ansi(&format_plan(
            color_mode,
            &plan,
            DetailLevel::Full,
            &HashMap::new(),
            Some(&schemas),
            &HashMap::new(),
            &[],
            &[],
            None,
            None,
        ))
    );
}

#[test]
fn route53_hosted_zone_name_strip_suffix_no_diff() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "route53_hosted_zone_name_strip_suffix_no_diff");
    assert!(
        plan.effects().is_empty(),
        "Route53 HostedZone state names with a trailing dot must compare equal to DSL names without one:\n{}",
        strip_ansi(&format_plan(
            color_mode,
            &plan,
            DetailLevel::Full,
            &HashMap::new(),
            Some(&schemas),
            &HashMap::new(),
            &[],
            &[],
            None,
            None,
        ))
    );
}

#[test]
fn snapshot_destroy_full() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture(color_mode, "destroy_full");
    let delete_attributes = delete_attributes_for_plan(&plan, &current_states);
    let output = strip_ansi(&format_destroy_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

/// carina#3356 (destroy-path counterpart of `snapshot_list_struct_child_gutter`):
/// a destroy plan whose non-last child carries a struck-through
/// `List<Struct>` value. Every physical row of the struck value must keep
/// the `│` tree gutter (the `strike_lines` + `reindent_with_gutter`
/// branch), and the strikethrough must not bleed across the gutter
/// (cf. #3115).
#[test]
fn snapshot_destroy_list_struct_child_gutter() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture(color_mode, "destroy_list_struct_child_gutter");
    let delete_attributes = delete_attributes_for_plan(&plan, &current_states);
    let output = strip_ansi(&format_destroy_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_destroy_orphans() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture(color_mode, "destroy_orphans");
    let delete_attributes = delete_attributes_for_plan(&plan, &current_states);
    let output = strip_ansi(&format_destroy_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_values() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "default_values");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_read_only_attrs() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "read_only_attrs");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_explicit() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "explicit");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Explicit,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_tags() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "default_tags");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_tags_update_replace() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "default_tags_update_replace");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_state_blocks() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "state_blocks");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_secret_values() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture(color_mode, "secret_values");
    let delete_attributes = delete_attributes_for_plan(&plan, &current_states);
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_nested_write_only() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fixture = build_plan_from_fixture_name(&runtime, "nested_write_only")
        .expect("fixture mock inputs must be valid");
    let historical = crate::commands::plan::collect_historical_display_attributes(
        &fixture.plan,
        &fixture.managed_current,
        fixture.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fixture.plan,
        DetailLevel::Full,
        &historical,
        Some(&fixture.schemas),
        &fixture.moved_origins,
        &[],
        &[],
        None,
        None,
    ));

    let unrelated = nested_write_only_listener_block(&output, "unrelated");
    let rotated = nested_write_only_listener_block(&output, "rotated");
    let replacement = nested_write_only_listener_block(&output, "replacement");
    let replacement_same_secret =
        nested_write_only_listener_block(&output, "replacement_same_secret");

    assert!(
        !unrelated.contains("default_actions"),
        "an unrelated update must hide the provenance-matching reassertion:\n{unrelated}"
    );
    assert!(
        !replacement_same_secret.contains("default_actions"),
        "a replacement must hide a provenance-matching nested secret:\n{replacement_same_secret}"
    );
    assert!(
        rotated.contains("default_actions"),
        "a nested secret rotation must render the bearing attribute:\n{rotated}"
    );
    assert!(
        replacement.contains("default_actions"),
        "a replacement with a nested secret rotation must render the bearing attribute:\n{replacement}"
    );
    assert_eq!(
        output.matches("default_actions").count(),
        2,
        "rotations should render, but unrelated reassertion and a provenance-matching replacement should not:\n{output}"
    );
    assert!(
        output.contains("client_secret") && output.contains("(secret)"),
        "the rotated leaf must remain visible but masked:\n{output}"
    );
    assert!(
        !output.contains("accepted-secret")
            && !output.contains("old-secret")
            && !output.contains("new-secret")
            && !output.contains("new-replace-secret"),
        "nested write-only plaintext must never reach plan output:\n{output}"
    );
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_moved_with_changes() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) = build_plan_from_fixture(color_mode, "moved_with_changes");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// Moved block with attribute removal: prev_desired_keys must transfer to new name.
///
/// State has "old_vpc" with desired_keys=["cidr_block", "tags"].
/// After move to "new_vpc", tags are removed from the DSL.
/// Plan should detect the removal via prev_desired_keys under the new name.
#[test]
fn snapshot_moved_prev_keys() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) = build_plan_from_fixture(color_mode, "moved_prev_keys");
    // The plan must contain an Update effect to remove the "tags" attribute.
    let has_update = plan
        .effects()
        .iter()
        .any(|e| matches!(e, carina_core::effect::Effect::Update { .. }));
    assert!(
        has_update,
        "Plan should detect tag removal via prev_desired_keys transfer, but no Update effect found"
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// Pure move: Move effect with no attribute changes.
///
/// State has "old_vpc" with cidr_block=10.0.0.0/16.
/// After move to "new_vpc", attributes are identical -> Move only, no Update.
/// The Move line must be visible in the plan tree.
#[test]
fn snapshot_moved_pure() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) = build_plan_from_fixture(color_mode, "moved_pure");
    // Pure move should NOT have an Update effect
    let has_update = plan
        .effects()
        .iter()
        .any(|e| matches!(e, carina_core::effect::Effect::Update { .. }));
    assert!(
        !has_update,
        "Pure move fixture should not produce an Update effect"
    );
    // But should have a Move effect
    let has_move = plan
        .effects()
        .iter()
        .any(|e| matches!(e, carina_core::effect::Effect::Move { .. }));
    assert!(has_move, "Pure move fixture should produce a Move effect");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

fn assert_pure_move_snapshot(
    color_mode: crate::output_style::ColorMode,
    fixture: &str,
    expected_loop_identity: &str,
) -> FixturePlan {
    let runtime = crate::test_support::TestCommandRuntime::new().with_color_mode(color_mode);
    let fp =
        build_plan_from_fixture_name(&runtime, fixture).expect("fixture mock inputs must be valid");
    let summary = fp.plan.summary();
    assert_eq!(summary.create, 0, "{fixture} unexpectedly creates");
    assert_eq!(summary.delete, 0, "{fixture} unexpectedly deletes");
    assert_eq!(summary.moved, 1, "{fixture} must contain one move");
    assert!(
        fp.moved_hints.is_empty(),
        "an explicit move must not produce a hint: {:?}",
        fp.moved_hints
    );
    assert!(
        fp.desired_resources
            .iter()
            .any(|resource| resource.resolved_id().identity_str() == expected_loop_identity),
        "{fixture} must materialize {expected_loop_identity} in desired resources"
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(format!("snapshot_{fixture}"), output);
    fp
}

fn moved_hint_snapshot_output(
    color_mode: crate::output_style::ColorMode,
    fp: &FixturePlan,
) -> String {
    let delete_attributes = crate::fixture_plan::delete_instance_attributes_from_plan(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let plan_output = format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        Some(&fp.prev_explicit),
        None,
    );
    strip_ansi(&format!("{plan_output}{}", fp.moved_hints))
}

fn assert_suggested_move_snapshot(
    color_mode: crate::output_style::ColorMode,
    fixture: &str,
    expected_from: &str,
    expected_to: &str,
) -> FixturePlan {
    let runtime = crate::test_support::TestCommandRuntime::new().with_color_mode(color_mode);
    let fp =
        build_plan_from_fixture_name(&runtime, fixture).expect("fixture mock inputs must be valid");
    assert!(
        fp.desired_resources
            .iter()
            .any(|resource| resource.resolved_id().identity_str() == expected_to),
        "{fixture} must materialize {expected_to} in desired resources"
    );
    let summary = fp.plan.summary();
    assert_eq!(summary.create, 1, "{fixture} must create one resource");
    assert_eq!(summary.delete, 1, "{fixture} must destroy one resource");
    assert_eq!(summary.moved, 0, "{fixture} must not contain a move");
    let [MovedBlockHint::Suggest(pair)] = fp.moved_hints.as_slice() else {
        panic!(
            "expected one move suggestion for {fixture}: {:?}",
            fp.moved_hints
        );
    };
    assert_eq!(pair.from().identity_str(), expected_from);
    assert_eq!(pair.to().identity_str(), expected_to);

    insta::assert_snapshot!(
        format!("snapshot_{fixture}"),
        moved_hint_snapshot_output(color_mode, &fp)
    );
    fp
}

fn assert_suggestion_round_trips(hint: &MovedBlockHint, expected_from: &str, expected_to: &str) {
    let rendered = hint.to_string();
    let block = rendered
        .lines()
        .skip_while(|line| *line != "  moved {")
        .take(5)
        .map(|line| line.strip_prefix("  ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!("provider aws {{}}\n\n{block}\n");
    let parsed = carina_core::parser::parse_file(
        &source,
        std::path::Path::new("module-moved-hint-round-trip.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .unwrap_or_else(|error| panic!("rendered block did not parse:\n{block}\n{error}"));
    assert!(
        parsed.warnings.is_empty(),
        "rendered block produced parser warnings:\n{block}\n{:?}",
        parsed.warnings
    );
    let [carina_core::parser::StateBlock::Moved { addresses }] = parsed.state_blocks.as_slice()
    else {
        panic!("expected one parsed moved block: {:?}", parsed.state_blocks);
    };
    let from = addresses.from();
    let to = addresses.to();
    assert_eq!(
        (from.provider.as_str(), from.resource_type.as_str()),
        ("aws", "route53.RecordSet")
    );
    assert_eq!(
        (to.provider.as_str(), to.resource_type.as_str()),
        ("aws", "route53.RecordSet")
    );
    assert_eq!(from.name_str(), expected_from);
    assert_eq!(to.name_str(), expected_to);
}

/// Moving a named resource into one element of a bound `for` expression is a
/// state-only move when the user supplies the exact expanded address.
#[test]
fn snapshot_moved_let_to_for_bound() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_pure_move_snapshot(color_mode, "moved_let_to_for_bound", "vpcs[0]");
}

/// An explicit move can target an unbound loop element by its stable anonymous
/// address and remains a state-only move.
#[test]
fn snapshot_moved_let_to_for_unbound() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_pure_move_snapshot(
        color_mode,
        "moved_let_to_for_unbound",
        "awscc_ec2_vpc_0bc2a2bb",
    );
}

/// Map iteration uses the canonical dot-form element address when the key is
/// a valid identifier.
#[test]
fn snapshot_moved_let_to_for_map() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_pure_move_snapshot(color_mode, "moved_let_to_for_map", "vpcs.a");
}

/// An unbound loop expanded from a resource reference accepts the same stable
/// anonymous move target as a literal-backed loop.
#[test]
fn snapshot_moved_let_to_for_unbound_reference() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_pure_move_snapshot(
        color_mode,
        "moved_let_to_for_unbound_reference",
        "awscc_ec2_vpc_0bc2a2bb",
    );
}

/// A loop expanded from a list held in live resource state accepts the same
/// explicit move and remains a pure state operation.
#[test]
fn snapshot_moved_let_to_for_read_loop() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_pure_move_snapshot(
        color_mode,
        "moved_let_to_for_read_loop",
        "validation_records[0]",
    );
}

/// The production certificate-validation rewrite also works inside a module
/// instance, preserving the module prefix on both moved addresses.
#[test]
fn snapshot_moved_let_to_for_read_loop_module() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_pure_move_snapshot(
        color_mode,
        "moved_let_to_for_read_loop_module",
        "r.validation_records[0]",
    );
}

/// Without a `moved` block the same rewrite remains a destroy plus create;
/// the diagnostic is appended to the ordinary plan output without changing
/// those effects.
#[test]
fn snapshot_moved_hint_let_to_for() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_suggested_move_snapshot(color_mode, "moved_hint_let_to_for", "old_vpc", "vpcs[0]");
}

/// Multiple matching destroys and creates form one connected component. The
/// warning must list the whole component and must not invent a pairing.
#[test]
fn snapshot_moved_hint_ambiguous() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "moved_hint_ambiguous")
        .expect("fixture mock inputs must be valid");
    let summary = fp.plan.summary();
    assert_eq!(summary.create, 2);
    assert_eq!(summary.delete, 2);
    assert_eq!(summary.moved, 0);
    let output = moved_hint_snapshot_output(color_mode, &fp);
    insta::assert_snapshot!(output);
    assert!(matches!(
        fp.moved_hints.as_slice(),
        [MovedBlockHint::Ambiguous(_)]
    ));
    assert!(
        !output.contains("moved {"),
        "ambiguous hint must not print a guessed moved block:\n{output}"
    );
}

/// The certificate-validation rewrite without a `moved` block must materialize
/// its loop child, replace the old record, and suggest the exact state move.
#[test]
fn snapshot_moved_hint_read_loop() {
    let color_mode = crate::output_style::ColorMode::Plain;
    assert_suggested_move_snapshot(
        color_mode,
        "moved_hint_read_loop",
        "validation_record",
        "validation_records[0]",
    );
}

/// Without an explicit move, a module-scoped read loop keeps the module prefix
/// in both the diagnostic and the parseable moved block it renders.
#[test]
fn snapshot_moved_hint_read_loop_module() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = assert_suggested_move_snapshot(
        color_mode,
        "moved_hint_read_loop_module",
        "r.validation_record",
        "r.validation_records[0]",
    );
    let [hint @ MovedBlockHint::Suggest(_)] = fp.moved_hints.as_slice() else {
        unreachable!("the shared helper asserted one suggestion")
    };
    assert_suggestion_round_trips(hint, "r.validation_record", "r.validation_records[0]");
}

/// Production-ordered fixture for RC2 claims precedence: the state row's
/// create-only values would make anonymous reconciliation adopt the old hash
/// name if the moved block did not claim it first.
#[test]
fn snapshot_moved_claims_precede_heuristics() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "moved_claims_precede_heuristics");
    assert_eq!(plan.summary().create, 0);
    assert_eq!(plan.summary().delete, 0);
    assert_eq!(plan.summary().moved, 1);
    assert!(
        plan.effects()
            .iter()
            .any(|e| matches!(e, carina_core::effect::Effect::Move { .. })),
        "claims-precedence fixture should produce a Move effect"
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// Compact (`DetailLevel::None`) rendering of a moved Update effect.
///
/// Locks in the `(moved from: <name>)` annotation form on the
/// compact path of `display::TreeRenderer::render_node` (#2470). The
/// detailed branch is already covered by `snapshot_moved_with_changes`;
/// this fixture-reusing test guards the compact branch separately so a
/// regression to the redundant `<provider>.<type>.<name>` form would
/// surface here instead of going unnoticed.
#[test]
fn snapshot_moved_with_changes_compact() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) = build_plan_from_fixture(color_mode, "moved_with_changes");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// Collect unused `let` bindings across every fixture subdirectory of
/// `fixtures_root`. A fixture is any immediate subdirectory containing at
/// least one `.crn` file (the file need not be named `main.crn` — sibling
/// layouts like `resources.crn` + `exports.crn` are covered).
fn collect_unused_let_bindings_in_fixtures(
    fixtures_root: &std::path::Path,
) -> Vec<(String, Vec<String>)> {
    let mut failures: Vec<(String, Vec<String>)> = Vec::new();

    for entry in std::fs::read_dir(fixtures_root).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let fixture_dir = entry.path();
        let has_crn = std::fs::read_dir(&fixture_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|ext| ext == "crn"));
        if !has_crn {
            continue;
        }
        let fixture_name = entry.file_name().to_string_lossy().to_string();
        let loaded = load_configuration(&fixture_dir).unwrap();
        let unused = carina_core::validation::check_unused_bindings(&loaded.authored_bindings);
        if unused.is_empty() {
            continue;
        }
        failures.push((fixture_name, unused));
    }

    failures
}

/// Ensure no fixture .crn file has unused `let` bindings.
///
/// `let` should only be used when a binding is referenced by another resource.
/// This test prevents regressions where unnecessary `let` bindings are added
/// to fixture files.
#[test]
fn no_unused_let_bindings_in_fixtures() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixtures_dir = PathBuf::from(format!("{}/tests/fixtures/plan_display", manifest_dir));

    let failures = collect_unused_let_bindings_in_fixtures(&fixtures_dir);

    if !failures.is_empty() {
        let msg: Vec<String> = failures
            .iter()
            .map(|(name, bindings)| {
                format!("  {}: unused let bindings: {}", name, bindings.join(", "))
            })
            .collect();
        panic!(
            "Fixture .crn files must not have unused let bindings:\n{}",
            msg.join("\n")
        );
    }
}

/// Regression test for #1997: before, the walker silently skipped any
/// fixture directory that did not contain `main.crn`, so unused `let`
/// bindings in a sibling-only layout (e.g. `resources.crn`) would pass
/// this check unnoticed. The walker now keys off "directory contains
/// any .crn file", so such layouts are exercised too.
#[test]
fn unused_let_check_covers_fixtures_without_main_crn() {
    let tmp = tempfile::tempdir().expect("snapshot fixture tempdir");
    let tmp_root = tmp.path();
    let fixture_dir = tmp_root.join("sibling_only");
    std::fs::create_dir_all(&fixture_dir).unwrap();

    // Deliberately NO main.crn. The only .crn declares an unused let binding.
    std::fs::write(
        fixture_dir.join("resources.crn"),
        "provider awscc {\n  region = awscc.Region.ap_northeast_1\n}\n\n\
         let orphan = awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n}\n",
    )
    .unwrap();

    let failures = collect_unused_let_bindings_in_fixtures(tmp_root);
    assert_eq!(
        failures.len(),
        1,
        "fixtures without main.crn must still be inspected for unused let bindings"
    );
    assert_eq!(failures[0].0, "sibling_only");
    assert_eq!(failures[0].1, vec!["orphan".to_string()]);
}

#[test]
fn plan_snapshot_upstream_state() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) = build_plan_from_fixture(color_mode, "upstream_state");
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

/// When an upstream_state's state file is missing, the plan must render
/// the unresolved attribute as `(known after upstream apply: <ref>)`
/// instead of leaving the raw dot-form (`network.vpc.vpc_id`) which
/// looks like a string literal. See issue #2366.
#[test]
fn plan_snapshot_upstream_state_unresolved() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "upstream_state_unresolved");
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    );
    let stripped = strip_ansi(&output);
    assert!(
        stripped.contains("(known after upstream apply: network.vpc.vpc_id)"),
        "expected unresolved upstream ref to render as `(known after upstream apply: ...)`, got:\n{}",
        stripped
    );
    insta::assert_snapshot!(stripped);
}

/// carina#3329 regression: an `import { id = "${X.attr}|…" }` whose
/// `${X.attr}` references a deferred upstream-state value must render
/// the interpolation through plan display as a `(known after upstream
/// apply: …)` placeholder around the literal segments rather than
/// silently substituting the `${…}` to empty.
///
/// Pre-#3329 the parser stored `id` as a plain `String` and discarded
/// every `${…}` segment, so the plan showed `id: |literal|literal` —
/// a malformed-looking identifier presented as if it were the cloud
/// API's real value. Operators reviewing the diff had no way to tell
/// whether apply would re-resolve the reference or ship the partial
/// string to AWS as-is.
#[test]
fn plan_snapshot_import_deferred_interpolation() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "import_deferred_interpolation");
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    );
    let stripped = strip_ansi(&output);
    assert!(
        stripped.contains("(known after upstream apply: management_route53.apex_zone_id)"),
        "expected the import id's `${{management_route53.apex_zone_id}}` segment to render \
         as `(known after upstream apply: management_route53.apex_zone_id)`, got:\n{}",
        stripped
    );
    assert!(
        !stripped.lines().any(|line| line.contains("id:")
            && line.contains("|registry-dev")
            && !line.contains("known after upstream apply")),
        "expected the leading `|` (caused by a silently-substituted ${{…}}) to be gone — \
         no `id:` line should show a bare `|registry-dev…` without the deferred marker. \
         Got:\n{}",
        stripped
    );
    insta::assert_snapshot!(stripped);
}

/// Companion to `plan_snapshot_upstream_state_unresolved`: state file is
/// present but `exports` is empty (upstream module declared but not yet
/// applied). The same `(known after upstream apply: <ref>)` rendering
/// must apply, with no warning since the state file was readable. See
/// issue #2366.
#[test]
fn plan_snapshot_upstream_state_empty_exports() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "upstream_state_empty_exports");
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    );
    let stripped = strip_ansi(&output);
    assert!(
        stripped.contains("(known after upstream apply: network.vpc.vpc_id)"),
        "expected empty-exports upstream ref to render as `(known after upstream apply: ...)`, got:\n{}",
        stripped
    );
    insta::assert_snapshot!(stripped);
}

/// Issue #2435: `let X = upstream_state {...}` lives in `state.crn`,
/// the consuming `${X.field['key']}` and bare `X.field['key']` live in
/// `main.crn`. The plan must resolve both forms to the concrete value
/// from the upstream's exports map.
#[test]
fn plan_snapshot_upstream_state_map_subscript() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "upstream_state_map_subscript");
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    );
    let stripped = strip_ansi(&output);
    // Both subscript forms — bare attribute value and inside `${...}`
    // interpolation — must end up substituted with the actual account ids.
    assert!(
        stripped.contains("222222222222"),
        "expected dev account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("111111111111"),
        "expected prod account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("shared-222222222222-bucket"),
        "expected `${{orgs.accounts['registry_dev']}}` interpolation substituted, got:\n{stripped}"
    );
    // Pin the bare-attribute form distinctly from the interpolation
    // form: a regression that re-rendered the bare-attribute case as
    // the literal `orgs.accounts['registry_dev']` (the original #2435
    // bug) would still satisfy the broad "contains the id" checks
    // above because the interpolation case would still substitute.
    assert!(
        stripped.contains("DevAccount: '222222222222'"),
        "expected bare `orgs.accounts['registry_dev']` attribute substituted, got:\n{stripped}"
    );
    insta::assert_snapshot!(stripped);
}

/// Issue #2447: companion to the subscript fixture, but for the
/// dot-notation form `${X.field.key}` / bare `X.field.key`. Pre-fix the
/// dot form passed validate but rendered the literal substring
/// `orgs.accounts.registry_dev` into the output (the parser fell back
/// to `Value::Concrete(ConcreteValue::String)` because the head wasn't a known binding in the
/// current file). Symmetric with the #2435 subscript fix; both forms
/// must now resolve to the upstream's concrete map value.
#[test]
fn plan_snapshot_upstream_state_map_dot_notation() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, moved_origins) =
        build_plan_from_fixture(color_mode, "upstream_state_map_dot_notation");
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
        None,
        None,
    );
    let stripped = strip_ansi(&output);
    // Both dot-notation forms — bare attribute value and inside `${...}`
    // interpolation — must end up substituted with the actual account ids.
    assert!(
        stripped.contains("222222222222"),
        "expected dev account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("111111111111"),
        "expected prod account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("shared-222222222222-bucket"),
        "expected `${{orgs.accounts.registry_dev}}` interpolation substituted, got:\n{stripped}"
    );
    // Pin that the bare-attribute form does not regress to the literal
    // `orgs.accounts.registry_dev` substring (the original #2447 bug).
    assert!(
        stripped.contains("DevAccount: '222222222222'"),
        "expected bare `orgs.accounts.registry_dev` attribute substituted, got:\n{stripped}"
    );
    assert!(
        !stripped.contains("orgs.accounts.registry_dev"),
        "literal `orgs.accounts.registry_dev` substring leaked into output:\n{stripped}"
    );
    insta::assert_snapshot!(stripped);
}

#[test]
fn plan_snapshot_exports() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::ExportChange;
    use carina_core::parser::TypeExpr;
    use carina_core::resource::Value;

    let (plan, schemas, moved_origins) = build_plan_from_fixture(color_mode, "exports");
    let export_changes = vec![
        ExportChange::Added {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::String),
            new_value: Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![])
                .resolve_root_for_test(),
        },
        ExportChange::Added {
            name: "cidr".to_string(),
            type_expr: None,
            new_value: Value::resource_ref("vpc".to_string(), "cidr_block".to_string(), vec![])
                .resolve_root_for_test(),
        },
    ];
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &export_changes,
        &[],
        None,
        None,
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

/// Verifies that a project whose provider/resource/exports blocks are
/// spread across sibling .crn files produces the same plan as the
/// single-file `exports` fixture. Guards against regressions where
/// directory-scoped parsing drops definitions in sibling files: the
/// `export_changes` fed to `format_plan` are derived from the loaded
/// `parsed.export_params`, so a dropped `exports.crn` would make the
/// Exports section disappear from the snapshot.
#[test]
fn plan_snapshot_exports_multifile() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name(&runtime, "exports_multifile")
        .expect("fixture mock inputs must be valid");

    // Assert the multi-file load actually picked up exports.crn before
    // rendering, so the snapshot claim is backed by parsed state.
    let exported_names: Vec<&str> = fp.export_params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        exported_names,
        vec!["vpc_id", "cidr"],
        "exports.crn definitions must be merged when loading a multi-file project"
    );

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &export_changes,
        &[],
        None,
        None,
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

#[test]
fn plan_snapshot_exports_multifile_let_literal() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name(&runtime, "exports_multifile_let_literal")
        .expect("fixture mock inputs must be valid");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &export_changes,
        &[],
        None,
        None,
    ));

    assert!(
        output.contains("bad_name = 'literal-via-let-chain'"),
        "bad_name export should render the sibling let literal, got:\n{}",
        output
    );
    assert!(
        output.contains("good_name = 'literal-via-resource-attr'"),
        "good_name export should render the sibling resource attribute, got:\n{}",
        output
    );
    for line in output
        .lines()
        .filter(|line| line.contains("bad_name =") || line.contains("good_name ="))
    {
        assert!(
            !line.contains("(known after apply)"),
            "literal exports must not be deferred placeholders, got line: {}",
            line
        );
    }

    insta::assert_snapshot!(output);
}

#[test]
fn plan_snapshot_exports_multifile_let_chain() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name(&runtime, "exports_multifile_let_chain")
        .expect("fixture mock inputs must be valid");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &export_changes,
        &[],
        None,
        None,
    ));

    assert!(
        output.contains("z = 'foo'"),
        "chained sibling let must resolve to the literal, got:\n{}",
        output
    );
    assert!(
        !output.contains("(known after apply)"),
        "no export should be deferred for the let-chain case, got:\n{}",
        output
    );

    insta::assert_snapshot!(output);
}

#[test]
fn plan_snapshot_exports_multifile_let_chain_5hop() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name(&runtime, "exports_multifile_let_chain_5hop")
        .expect("fixture mock inputs must be valid");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &export_changes,
        &[],
        None,
        None,
    ));

    assert!(
        output.contains("z = 'x'"),
        "5-hop sibling let chain must resolve to the literal, got:\n{}",
        output
    );
    assert!(
        !output.contains("(known after apply)"),
        "no export should be deferred for the 5-hop let-chain case, got:\n{}",
        output
    );

    insta::assert_snapshot!(output);
}

#[test]
fn plan_exports_multifile_resource_attribute_ref_resolves() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name(&runtime, "exports_multifile_resource_attribute_ref")
        .expect("fixture mock inputs must be valid");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &export_changes,
        &[],
        None,
        None,
    ));

    assert!(
        output.contains("x = \"fixture-instance\"") || output.contains("x = 'fixture-instance'"),
        "the sibling resource attribute must resolve through the module output, got:\n{}",
        output
    );
}

#[test]
fn plan_exports_multifile_module_output_resolves() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name(&runtime, "exports_multifile_module_output")
        .expect("fixture mock inputs must be valid");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &export_changes,
        &[],
        None,
        None,
    ));

    assert!(
        output.contains("z = 'x'") || output.contains("z = \"x\""),
        "the module call output must resolve, got:\n{}",
        output
    );
}

#[test]
fn plan_snapshot_exports_multifile_string_let_attr_access_rejected() {
    let user_dirs = crate::test_support::TestCommandRuntime::new();
    let fixture_path = PathBuf::from(format!(
        "{}/tests/fixtures/plan_display/exports_multifile_string_let_attr_access_rejected",
        env!("CARGO_MANIFEST_DIR")
    ));
    let loaded = load_configuration(&fixture_path).unwrap();
    let duplicate_declarations = loaded.duplicate_declarations;
    let parsed = loaded.parsed;
    let providers =
        crate::wiring::ProviderLoad::load(&user_dirs, &parsed.providers, &fixture_path).unwrap();
    let errors = crate::commands::validate_and_resolve_errors(
        &parsed,
        &fixture_path,
        &providers,
        true,
        &duplicate_declarations,
    );
    let message = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !errors.is_empty(),
        "field access on a sibling string let must be rejected"
    );
    assert!(
        message.contains("cannot apply access .foo to string"),
        "expected scalar field-access diagnostic naming the failed step and shape, got:\n{}",
        message
    );
    assert!(
        !message.contains("(known after apply)"),
        "scalar field-access rejection must not degrade to deferred output, got:\n{}",
        message
    );
}

#[test]
fn plan_snapshot_export_changes_mixed() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use crate::commands::plan::ExportChange;
    use carina_core::parser::TypeExpr;
    use carina_core::resource::{ConcreteValue, Value};

    let (plan, schemas, moved_origins) = build_plan_from_fixture(color_mode, "no_changes");
    let export_changes = vec![
        ExportChange::Added {
            name: "new_export".to_string(),
            type_expr: Some(TypeExpr::String),
            new_value: Value::Concrete(ConcreteValue::String("hello".into())),
        },
        ExportChange::Modified {
            name: "changed".to_string(),
            type_expr: Some(TypeExpr::Int),
            old_json: serde_json::from_value(serde_json::json!(42)).unwrap(),
            new_value: Value::Concrete(ConcreteValue::Int(100)),
        },
        ExportChange::Removed {
            name: "obsolete".to_string(),
            old_json: serde_json::from_value(serde_json::json!("gone")).unwrap(),
        },
    ];
    let output = format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &export_changes,
        &[],
        None,
        None,
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

#[test]
fn snapshot_nested_map_diff() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "nested_map_diff");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

// #2877 acceptance: a struct element added to a list-of-maps attribute
// must render with one field per line (multi-line `+ { ... }` block),
// not as an inline single-line dump. The pre-fix path collapsed the
// added element to a `+ {action: [...], effect: "...", ...}` line that
// blew past 500 columns for IAM policy statements.
#[test]
fn snapshot_list_diff_added_struct() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "list_diff_added_struct");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        !output.lines().any(|l| l.len() > 200),
        "added struct element should not render as a single very wide line; got: {}",
        output
    );
    insta::assert_snapshot!(output);
}

/// carina#3356 (diff-path counterpart of `snapshot_list_struct_child_gutter`):
/// a `~` update that adds a struct element to a `List<Struct>` on a
/// resource nested as a non-last child in the tree. The added-element
/// `+ { ... }` block and every field row inside it must carry the `│`
/// tree gutter, not bare-space indentation. Pre-fix
/// `render_added_removed_block` laid the field rows out with plain
/// spaces, so the diff block floated outside the tree under a gutter.
#[test]
fn snapshot_list_diff_added_struct_child_gutter() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "list_diff_added_struct_child_gutter");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

// #2881 acceptance: when a list-of-maps element is modified (paired with
// an old element by similarity), unchanged fields inside the `~ { ... }`
// block should be hidden behind a single `# (n unchanged fields hidden)`
// summary in Full mode — mirroring the top-level
// `# (n unchanged attributes hidden)` convention. Pre-fix every sibling
// field rendered next to the changed one, including a long
// `action: [...]` list that produced a wide one-line dump.
#[test]
fn snapshot_list_diff_modified_with_unchanged() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "list_diff_modified_with_unchanged");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        !output.lines().any(|l| l.len() > 200),
        "modified list-element block should not render a wide unchanged-field line; got: {}",
        output
    );
    assert!(
        output.contains("unchanged field"),
        "expected `# (n unchanged fields hidden)` summary in Full mode; got: {}",
        output
    );
    insta::assert_snapshot!(output);
}

/// #3722: a statement inserted before a modified peer must remain an addition.
/// All unmatched statements share `effect = "Allow"`, so the inserted element
/// can greedily steal either saved statement unless pairing globally commits
/// the higher-score peers first.
#[test]
fn snapshot_list_diff_best_first_mid_insert() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "list_diff_best_first_mid_insert");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        output.contains("sid: 'ECRRepository'"),
        "inserted statement must render as an added element: {output}"
    );
    assert!(
        output.contains("arn:aws:iam::*:role/carina-registry-publish-api-execution"),
        "existing statement's added ARN must remain visible: {output}"
    );
    assert!(
        !output.contains("'ManagePublishStackRoles' →"),
        "best-first pairing must not render the existing SID as renamed: {output}"
    );
    insta::assert_snapshot!(output);
}

/// #2943: a List<String> field inside a list-of-maps modified element
/// that grew by trailing entries must render multi-line with `+` lines
/// for added entries, not as a single inline `field: [a, b, ...] →
/// [a, b, ..., c, d]` overflow line.
#[test]
fn snapshot_list_diff_string_list_grew() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "list_diff_string_list_grew");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        !output.lines().any(|l| l.len() > 200),
        "string-list diff inside a list-of-maps modified element should not render inline; got: {}",
        output
    );
    insta::assert_snapshot!(output);
}

/// #3234: a `List<String>` field nested inside a Map (`principal`) which
/// is itself inside a list-of-maps element (`statement`) must render the
/// list growth multi-line with `+` markers, not as a single inline
/// `aws: [...] → [...]` overflow line. The pre-fix `MapDiffEntryIR`
/// only had `Changed` for the non-map / non-list-of-maps fallthrough,
/// which collapsed long IAM principal lists onto one unscannable line.
#[test]
fn snapshot_map_field_string_list_grew() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "map_field_string_list_grew");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        !output.lines().any(|l| l.len() > 200),
        "string-list diff inside a nested map field should not render inline; got: {}",
        output
    );
    insta::assert_snapshot!(output);
}

// #2881 follow-on: exercises the modified-with-nested branch where the
// only changed field is a nested Map (`config`). Locks in the
// `~ { ... }` block-style layout: nested map diff first, then
// `# (n unchanged fields hidden)` summary inside the block, then the
// closing `}` on its own line. This is the round-1-review edge case
// where the inline-spans buffer gets flushed mid-iteration by the
// `NestedMapChanged` arm.
#[test]
fn snapshot_list_diff_modified_with_unchanged_nested() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "list_diff_modified_with_unchanged_nested");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        !output.lines().any(|l| l.len() > 200),
        "modified list-element block should not render a wide unchanged-field line; got: {}",
        output
    );
    assert!(
        output.contains("unchanged field"),
        "expected `# (n unchanged fields hidden)` summary in Full mode; got: {}",
        output
    );
    insta::assert_snapshot!(output);
}

// #2886: paired list-of-maps elements whose only "diff" was an
// upstream-injected key dropped by the IR are absorbed into the
// trailing `# (n unchanged attributes hidden)` summary. Snapshot
// pins the resulting per-resource block (no `statement:` row, count
// includes the absorbed attribute).
#[test]
fn snapshot_list_diff_paired_all_unchanged_dropped() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "list_diff_paired_all_unchanged_dropped");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

// #2910: a Map attribute whose only "diff" is a nested list-of-maps
// where every paired element is dropped (per #2886). Pre-fix the tree
// rendered dangling section headers (`config:`, `settings:`, `rules:`)
// with nothing under them. Post-fix the IR drops the empty sections
// recursively and the parent Map row is absorbed into the trailing
// unchanged-attributes count.
#[test]
fn snapshot_nested_list_of_maps_all_dropped() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) =
        build_plan_from_fixture(color_mode, "nested_list_of_maps_all_dropped");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

// Mirror of the added-struct test for the removed path.
#[test]
fn snapshot_list_diff_removed_struct() {
    let color_mode = crate::output_style::ColorMode::Plain;
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "list_diff_removed_struct");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    assert!(
        !output.lines().any(|l| l.len() > 200),
        "removed struct element should not render as a single very wide line; got: {}",
        output
    );
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "deferred_for")
        .expect("fixture mock inputs must be valid");
    assert!(
        !fp.deferred_for_expressions.is_empty(),
        "expected at least one deferred for-expression"
    );

    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &fp.deferred_for_expressions,
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for_create_solo() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "deferred_for_create_solo")
        .expect("fixture mock inputs must be valid");
    let delete_attributes = crate::fixture_plan::delete_instance_attributes_from_plan(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &fp.deferred_for_expressions,
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for_anonymous() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "deferred_for_anonymous")
        .expect("fixture mock inputs must be valid");
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &fp.deferred_for_expressions,
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for_with_paired_destroy() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "deferred_for_with_paired_destroy")
        .expect("fixture mock inputs must be valid");
    let delete_attributes = crate::fixture_plan::delete_instance_attributes_from_plan(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &fp.deferred_for_expressions,
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for_with_dependent_wait() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let mut fp = build_plan_from_fixture_name(&runtime, "deferred_for_with_dependent_wait")
        .expect("fixture mock inputs must be valid");
    let mut plan = Plan::new();
    for effect in fp.plan.effects() {
        let effect = match effect.clone() {
            Effect::Create(resource)
                if resource.id.resource_type == "acm.CertificateValidation" =>
            {
                let mut resource = resource.into_inner();
                resource.directives.depends_on.push(
                    carina_core::resource::absolute_binding_address_for_test("validation_records"),
                );
                Effect::Create(resolved(resource))
            }
            effect => effect,
        };
        plan.add(effect);
    }
    fp.plan = plan;
    let delete_attributes = crate::fixture_plan::delete_instance_attributes_from_plan(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &fp.deferred_for_expressions,
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for_with_unrelated_delete() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "deferred_for_with_unrelated_delete")
        .expect("fixture mock inputs must be valid");
    let delete_attributes = crate::fixture_plan::delete_instance_attributes_from_plan(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &fp.deferred_for_expressions,
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_provider_prefix() {
    let color_mode = crate::output_style::ColorMode::Plain;
    // Regression guard for #2426 / #2431: anonymous resource identifiers
    // gain a `<provider>_` prefix so plan output and state files
    // self-describe their provider. The header line for the lone Vpc
    // resource must read `+ awscc.ec2.Vpc awscc_ec2_vpc_<8hex>`.
    let (plan, schemas, _moved) = build_plan_from_fixture(color_mode, "provider_prefix");
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));
    insta::assert_snapshot!(output);
}

/// Refs awscc#206. The user wrote `tags = { Env = 'prod' }`; the
/// saved state's actual `tags` value carries an additional `Name`
/// leaf the user never authored. With prev_explicit threaded into
/// the differ + display, projection through the authoring tree
/// strips the unauthored `Name` leaf before comparison — there
/// must be no Update effect for the Vpc and no `- Name: ...` line
/// in the rendered plan.
#[test]
fn snapshot_server_default_struct_leaf() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "server_default_struct_leaf")
        .expect("fixture mock inputs must be valid");
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        Some(&fp.prev_explicit),
        None,
    ));
    insta::assert_snapshot!(output);

    // Acceptance: no `Name` line should appear in the rendered plan,
    // since `Name` is a server-side default the user never authored.
    assert!(
        !output.contains("Name"),
        "rendered plan must not surface the unauthored 'Name' leaf, got:\n{}",
        output
    );
}

/// carina-provider-awscc#511. The persisted list contains one authored
/// `{ key, value }` element followed by two service-added defaults. Changing
/// the authored value must render only that element; defaults indexed by
/// `ExplicitFields::ListElements::Unrecorded` must not appear as removals.
#[test]
fn snapshot_server_added_list_elements_changed() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "server_added_list_elements_changed")
        .expect("fixture mock inputs must be valid");
    let historical = crate::commands::plan::collect_historical_display_attributes(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &historical,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        Some(&fp.prev_explicit),
        None,
    ));

    assert!(
        output.contains("'60' → '30'"),
        "authored change missing from plan output:\n{output}"
    );
    assert!(
        !output.contains("stickiness.type") && !output.contains("load_balancing.algorithm.type"),
        "server-added elements must not render as removals:\n{output}"
    );
    insta::assert_snapshot!(output);
}

/// carina#4118. Omitting an authored keyed partial-update element releases
/// its authoring record instead of promising a provider-side deletion.
#[test]
fn snapshot_keyed_partial_update_release() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "keyed_partial_update_release")
        .expect("fixture mock inputs must be valid");
    let historical = crate::commands::plan::collect_historical_display_attributes(
        &fp.plan,
        &fp.managed_current,
        fp.state_file.as_ref(),
    );
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::Full,
        &historical,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        Some(&fp.prev_explicit),
        None,
    ));

    assert!(
        output.contains(
            "! target_group_attributes[\"b\"]: left as '2' (AWS does not revert omitted keys; write the default value to revert)"
        ),
        "key release row missing from plan output:\n{output}"
    );
    assert!(
        output.contains("Plan: 0 to add, 0 to change, 0 to destroy, 1 to release."),
        "release summary missing from plan output:\n{output}"
    );
    assert!(
        !output.contains("stickiness.type") && !output.contains("load_balancing.algorithm.type"),
        "server-added elements must stay hidden:\n{output}"
    );
    insta::assert_snapshot!(output);
}

/// carina#3307 + carina#3322 acceptance: when an `ExpansionTrace`
/// records two leaf resources under a composition call site, the
/// rendered plan groups them under a
/// `module "<binding>" (<source_path>)` header. The third leaf,
/// declared at the DSL root with no trace entry, stays at the top
/// level. The header label was rebadged from the internal
/// `Composition "<binding>"` term in carina#3322.
#[test]
fn snapshot_composition_folding() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::{
        CallSite, EphemeralId, ExpansionTrace, PersistentId, Resource, ResourceId,
    };
    use carina_core::schema::SchemaRegistry;

    let mut plan = Plan::new();

    // Two leaves under the composition `cluster`.
    let inner = Resource::new("aws.eks.Cluster", "cluster/inner");
    let inner_role = Resource::new("aws.iam.Role", "cluster/inner-role");
    // One leaf at the DSL root.
    let logs = Resource::new("aws.s3.Bucket", "logs");

    let inner_id = inner.id.clone();
    let inner_role_id = inner_role.id.clone();

    plan.add(Effect::Create(resolved(inner)));
    plan.add(Effect::Create(resolved(inner_role)));
    plan.add(Effect::Create(resolved(logs)));

    let mut trace = ExpansionTrace::new();
    // carina#3322: the call site carries the `use { source = "..." }`
    // path so the rendered header reads
    // `module "cluster" (./modules/cluster)`.
    let cluster_site = CallSite::new(
        EphemeralId::new(carina_core::resource::ResolvedResourceId::new(
            ResourceId::with_identity("_virtual", "cluster"),
        )),
        "./modules/cluster",
    );
    trace.record(PersistentId::new(inner_id), vec![cluster_site.clone()]);
    trace.record(PersistentId::new(inner_role_id), vec![cluster_site]);

    let schemas = SchemaRegistry::new();
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
        None,
        Some(&trace),
    ));
    insta::assert_snapshot!(output);

    // Acceptance: the rebadged module header must appear, with the
    // DSL-visible source path in parens, and the two leaves following
    // it in the output (carina#3322).
    assert!(
        output.contains("module \"cluster\" (./modules/cluster)"),
        "expected `module \"cluster\" (./modules/cluster)` header in:\n{}",
        output
    );
    assert!(
        !output.contains("Composition"),
        "must not leak internal `Composition` term in:\n{}",
        output
    );
    assert!(
        output.contains("cluster/inner"),
        "expected inner leaf row in:\n{}",
        output
    );
    assert!(
        output.contains("logs"),
        "expected ungrouped `logs` leaf row in:\n{}",
        output
    );
}

/// carina#3593 acceptance: top-level resource rows, data-source rows,
/// and folded module headers share one sigil column and one content
/// column. Narrow sigils are padded after the sigil; wider sigils keep
/// their natural width.
#[test]
fn snapshot_top_level_sigil_alignment() {
    let color_mode = crate::output_style::ColorMode::Plain;
    use carina_core::effect::Effect;
    use carina_core::parser::{DeferredForExpression, ForBinding};
    use carina_core::plan::Plan;
    use carina_core::resource::{
        CallSite, DataSource, EphemeralId, ExpansionTrace, PersistentId, Resource, ResourceId,
        Value,
    };
    use carina_core::schema::SchemaRegistry;

    let mut plan = Plan::new();

    let bucket = Resource::new("aws.s3.Bucket", "logs");
    let roles = DataSource::new("aws.iam.Roles", "admin_roles");
    let cluster = Resource::new("aws.eks.Cluster", "cluster/inner");
    let cluster_id = cluster.id.clone();

    plan.add(Effect::Create(resolved(bucket)));
    plan.add(Effect::Read {
        resource: resolved_data_source(roles),
    });
    plan.add(Effect::Create(resolved(cluster)));

    let mut deferred_template: Resource<
        carina_core::resource::ModuleSource,
        carina_core::resource::LoopSlot,
    > = Resource::new("aws.route53.RecordSet", "validation_records");
    deferred_template.set_attr(
        "name",
        Value::<
            carina_core::resource::LoopSlot,
            carina_core::resource::RelativeBindingName,
        >::resource_ref("option", "name", vec![])
        .resolve_root_for_test(),
    );
    let deferred = DeferredForExpression::new(
        None,
        1,
        "for option in cert.domain_validation_options".to_string(),
        carina_core::parser::DeferredForTarget::bound(
            carina_core::resource::absolute_binding_address_for_test("validation_records"),
        )
        .expect("fixture binding is valid"),
        Value::resource_ref("cert", "domain_validation_options", Vec::new())
            .resolve_root_for_test(),
        ForBinding::Simple("option".to_string()),
        deferred_template,
    );
    let deferred_for_expressions = vec![deferred];

    let mut trace = ExpansionTrace::new();
    let cluster_site = CallSite::new(
        EphemeralId::new(carina_core::resource::ResolvedResourceId::new(
            ResourceId::with_identity("_virtual", "cluster"),
        )),
        "./modules/cluster",
    );
    trace.record(PersistentId::new(cluster_id), vec![cluster_site]);

    let schemas = SchemaRegistry::new();
    let output = strip_ansi(&format_plan(
        color_mode,
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &deferred_for_expressions,
        None,
        Some(&trace),
    ));
    insta::assert_snapshot!(output);

    let rows = [
        ("+", "aws.s3.Bucket", "create resource"),
        ("<=", "aws.iam.Roles", "read data source"),
        ("+", "aws.route53.RecordSet", "deferred for-expression"),
        ("▾", "module", "module header"),
    ];
    for (sigil, content, label) in rows {
        let line = output
            .lines()
            .find(|line| line.contains(content))
            .unwrap_or_else(|| panic!("missing {label} row in:\n{output}"));
        let expected_prefix = format!("  {sigil} ");
        assert_eq!(
            line.chars()
                .position(|c| c == sigil.chars().next().unwrap()),
            Some(2),
            "{label} sigil must start at column 2: {line:?}",
        );
        assert!(
            line.starts_with(&expected_prefix),
            "{label} row must use a single separator after its sigil: {line:?}",
        );
    }
}

/// carina#3322 end-to-end: loading a real multi-file fixture
/// (`module_anonymous_resource` — main.crn + an `oidc-module/`
/// subdirectory referenced as `use { source = './oidc-module' }`)
/// must render the composition group with `module "<binding>"
/// (./oidc-module)`. This exercises the full pipeline:
/// `process_imports` records `module_paths`, `expand_module_call`
/// stamps each `CallSite`, the renderer reads `source_path`. Without
/// this test the rebadge could regress silently for any input the
/// hand-built `snapshot_composition_folding` doesn't cover.
///
/// Also obeys the CLAUDE.md "directory-scoped, never single-file"
/// policy: the fixture is a real two-directory module shape, not a
/// bare string.
#[test]
fn snapshot_module_header_renders_use_source_path_for_real_fixture() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let color_mode = crate::output_style::ColorMode::Plain;
    let fp = build_plan_from_fixture_name(&runtime, "module_anonymous_resource")
        .expect("fixture mock inputs must be valid");
    let output = strip_ansi(&format_plan(
        color_mode,
        &fp.plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &[],
        None,
        Some(&fp.expansion_trace),
    ));

    assert!(
        output.contains(r#"▾ module "bootstrap" (./oidc-module)"#),
        "expected `▾ module \"bootstrap\" (./oidc-module)` header from real \
         fixture in:\n{}",
        output,
    );
    assert!(
        !output.contains("Composition"),
        "internal `Composition` term must not surface to operators in:\n{}",
        output,
    );
}

#[test]
fn round2_saved_plan_matches_existing_nested_and_tags_snapshots() {
    for fixture in [
        "nested_map_diff",
        "map_key_diff",
        "list_diff_modified_with_unchanged_nested",
        "list_diff_paired_all_unchanged_dropped",
        "nested_list_of_maps_all_dropped",
    ] {
        let runtime = crate::test_support::TestCommandRuntime::new();
        let fp = build_plan_from_fixture_name(&runtime, fixture)
            .expect("fixture mock inputs must be valid");
        let wire = serde_json::to_vec(&carina_core::plan::SavedPlan::from(fp.plan)).unwrap();
        let saved: carina_core::plan::UnresolvedSavedPlan = serde_json::from_slice(&wire).unwrap();
        let plan = saved.resolve_with_schemas(&fp.schemas).unwrap();
        let provider =
            provider_router_for_plan(&runtime, &plan).expect("fixture mock inputs must be valid");
        let summary = PlanChangeSummary::new(&plan, &[], &fp.deferred_for_expressions, &[]);
        let output = format_plan_with_delete_instances(
            runtime.color_mode(),
            &plan,
            &summary,
            DetailLevel::Full,
            &carina_core::detail_rows::HistoricalDisplayAttributes::for_plan(&plan),
            Some(&fp.schemas),
            &HashMap::new(),
            None,
            None,
            &provider,
            carina_core::detail_rows::DetailComparisons::RecordedEffects,
        )
        .unwrap();
        let expected = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("src/snapshots")
                .join(format!(
                    "carina_cli__plan_snapshot_tests__snapshot_{fixture}.snap"
                )),
        )
        .unwrap();
        let expected = expected
            .splitn(3, "---\n")
            .nth(2)
            .expect("snapshot body")
            .trim_end();
        assert_eq!(
            strip_ansi(&output).trim_end(),
            expected,
            "saved-plan fixture {fixture}"
        );
    }
}
