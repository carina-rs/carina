//! Plan display snapshot tests.
//!
//! Each test loads a .crn fixture (and optionally a state file), builds a plan
//! using the same logic as `--refresh=false`, formats the plan output, strips
//! ANSI color codes, and asserts the result against an `insta` snapshot.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use carina_core::config_loader::load_configuration;
use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::resource::{ResourceId, State};
use carina_core::schema::SchemaRegistry;

use crate::DetailLevel;
use crate::display::{format_destroy_plan, format_plan};
use crate::fixture_plan::build_plan_from_fixture_name;

/// Strip ANSI escape codes from a string for snapshot readability.
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

/// Build a plan from a .crn fixture and optional state file, mimicking `--refresh=false`.
fn build_plan_from_fixture(
    fixture_dir: &str,
) -> (
    carina_core::plan::Plan,
    SchemaRegistry,
    HashMap<ResourceId, ResourceId>,
) {
    let fp = build_plan_from_fixture_name(fixture_dir);
    (fp.plan, fp.schemas, fp.moved_origins)
}

#[allow(clippy::type_complexity)]
fn build_plan_and_states_from_fixture(
    fixture_dir: &str,
) -> (
    carina_core::plan::Plan,
    HashMap<ResourceId, State>,
    SchemaRegistry,
    HashMap<ResourceId, ResourceId>,
) {
    let fp = build_plan_from_fixture_name(fixture_dir);
    (fp.plan, fp.current_states, fp.schemas, fp.moved_origins)
}

/// Plan-display gate for `directives.depends_on` (#2823). The bucket
/// declares an explicit ordering edge to `role` with no value
/// reference; the snapshot pins how the plan tree renders that edge.
#[test]
fn snapshot_depends_on() {
    let (plan, schemas, _moved) = build_plan_from_fixture("depends_on");
    let output = strip_ansi(&format_plan(
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
fn snapshot_iam_preflight_warning_appears_after_plan_summary() {
    let (plan, schemas, _moved) = build_plan_from_fixture("depends_on");
    let mut output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("wait_cert");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("all_create");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("multi_instance_create");
    let output = strip_ansi(&format_plan(
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
    let (plan, _schemas, _moved) = build_plan_from_fixture("multi_instance_create");

    let mut by_name: HashMap<String, Option<String>> = HashMap::new();
    for effect in plan.effects() {
        let id = effect.resource_id();
        by_name.insert(id.name_str().to_string(), id.provider_instance.clone());
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
    let (plan, _schemas, _moved) = build_plan_from_fixture("multi_instance_module");

    let routed: Vec<(String, Option<String>)> = plan
        .effects()
        .iter()
        .map(|e| {
            let id = e.resource_id();
            (id.name_str().to_string(), id.provider_instance.clone())
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
    let (plan, schemas, _moved) = build_plan_from_fixture("module_anonymous_resource");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("policy_pretty");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("policy_pretty_nested");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("list_struct_child_gutter");
    let output = strip_ansi(&format_plan(
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
    // #2528 acceptance: an IAM trust-policy `condition.<op>.<context-key>:
    // [list]` shape — a multi-element list-of-strings nested under a
    // dynamic-key Map — must break across lines all the way down, the
    // same way `statement[].action` does. Pre-fix the deepest list
    // collapsed to one line because the inline-vs-vertical decision did
    // not bubble down to nested Map values.
    let (plan, schemas, _moved) = build_plan_from_fixture("policy_pretty_dynamic_key_list");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("pretty_long_string_list");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("pretty_short_string_list");
    let output = strip_ansi(&format_plan(
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
    let (plan, _schemas, _moved) = build_plan_from_fixture("no_changes");
    let output = strip_ansi(&format_plan(
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
    let fp = build_plan_from_fixture_name("empty_explicit_children_no_changes");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("mixed_operations");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("replace_create_only");
    let output = strip_ansi(&format_plan(
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
fn snapshot_replace_with_non_forcing_diffs() {
    let (plan, schemas, _moved) = build_plan_from_fixture("replace_with_non_forcing_diffs");
    let output = strip_ansi(&format_plan(
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
    use carina_core::resource::Value;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture("delete_orphan");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let carina_core::effect::Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();
    let output = strip_ansi(&format_plan(
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

/// A deleted orphan whose state carries a list-of-maps attribute
/// (`domain_validation_options`) must render that attribute vertically
/// (multi-line), not on one long unreadable line. Regression for the
/// Delete path being asymmetric with the Create path in
/// `build_detail_rows` / `build_delete_rows`.
#[test]
fn snapshot_delete_orphan_list_of_maps() {
    use carina_core::resource::Value;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture("delete_orphan_list_of_maps");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let carina_core::effect::Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();
    let output = strip_ansi(&format_plan(
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
    let (plan, _schemas, _moved) = build_plan_from_fixture("compact");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("map_key_diff");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("map_added_from_none");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("map_attribute_removed");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("enum_display");
    let output = strip_ansi(&format_plan(
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
fn snapshot_no_changes_enum() {
    let (plan, _schemas, _moved) = build_plan_from_fixture("no_changes_enum");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("dynamic_enum_az_no_diff");
    assert!(
        plan.effects().is_empty(),
        "API-spelled saved state must compare equal to DSL-spelled desired state through the CLI plan path:\n{}",
        strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) =
        build_plan_from_fixture("route53_hosted_zone_name_strip_suffix_no_diff");
    assert!(
        plan.effects().is_empty(),
        "Route53 HostedZone state names with a trailing dot must compare equal to DSL names without one:\n{}",
        strip_ansi(&format_plan(
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
    use carina_core::resource::Value;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture("destroy_full");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = current_states
        .into_iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id, state.attributes))
        .collect();
    let output = strip_ansi(&format_destroy_plan(
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
    use carina_core::resource::Value;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture("destroy_list_struct_child_gutter");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = current_states
        .into_iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id, state.attributes))
        .collect();
    let output = strip_ansi(&format_destroy_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_destroy_orphans() {
    use carina_core::resource::Value;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture("destroy_orphans");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = current_states
        .into_iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id, state.attributes))
        .collect();
    let output = strip_ansi(&format_destroy_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_values() {
    let (plan, schemas, _moved) = build_plan_from_fixture("default_values");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("read_only_attrs");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("explicit");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("default_tags");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("state_blocks");
    let output = strip_ansi(&format_plan(
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
    use carina_core::resource::Value;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture("secret_values");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let carina_core::effect::Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();
    let output = strip_ansi(&format_plan(
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
fn snapshot_moved_with_changes() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_with_changes");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_prev_keys");
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_pure");
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

/// Production-ordered fixture for RC2 claims precedence: the state row's
/// create-only values would make anonymous reconciliation adopt the old hash
/// name if the moved block did not claim it first.
#[test]
fn snapshot_moved_claims_precede_heuristics() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_claims_precede_heuristics");
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_with_changes");
    let output = strip_ansi(&format_plan(
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
        let unused = crate::wiring::check_unused_bindings(&loaded.unresolved_parsed);
        if unused.is_empty() {
            continue;
        }
        // Moved block targets are structurally required bindings
        let move_targets: HashSet<String> = loaded
            .unresolved_parsed
            .state_blocks
            .iter()
            .filter_map(|sb| {
                if let carina_core::parser::StateBlock::Moved { to, .. } = sb {
                    Some(to.name_str().to_string())
                } else {
                    None
                }
            })
            .collect();
        let truly_unused: Vec<String> = unused
            .into_iter()
            .filter(|b| !move_targets.contains(b))
            .collect();
        if !truly_unused.is_empty() {
            failures.push((fixture_name, truly_unused));
        }
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
    let tmp_root = std::env::temp_dir().join("carina_test_unused_let_sibling_only");
    let _ = std::fs::remove_dir_all(&tmp_root);
    let fixture_dir = tmp_root.join("sibling_only");
    std::fs::create_dir_all(&fixture_dir).unwrap();

    // Deliberately NO main.crn. The only .crn declares an unused let binding.
    std::fs::write(
        fixture_dir.join("resources.crn"),
        "provider awscc {\n  region = awscc.Region.ap_northeast_1\n}\n\n\
         let orphan = awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n}\n",
    )
    .unwrap();

    let failures = collect_unused_let_bindings_in_fixtures(&tmp_root);
    assert_eq!(
        failures.len(),
        1,
        "fixtures without main.crn must still be inspected for unused let bindings"
    );
    assert_eq!(failures[0].0, "sibling_only");
    assert_eq!(failures[0].1, vec!["orphan".to_string()]);

    let _ = std::fs::remove_dir_all(&tmp_root);
}

#[test]
fn plan_snapshot_upstream_state() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state");
    let output = format_plan(
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_unresolved");
    let output = format_plan(
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("import_deferred_interpolation");
    let output = format_plan(
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_empty_exports");
    let output = format_plan(
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_map_subscript");
    let output = format_plan(
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
        stripped.contains("DevAccount: \"222222222222\""),
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
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_map_dot_notation");
    let output = format_plan(
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
        stripped.contains("DevAccount: \"222222222222\""),
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
    use crate::commands::plan::ExportChange;
    use carina_core::parser::TypeExpr;
    use carina_core::resource::Value;

    let (plan, schemas, moved_origins) = build_plan_from_fixture("exports");
    let export_changes = vec![
        ExportChange::Added {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::String),
            new_value: Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        },
        ExportChange::Added {
            name: "cidr".to_string(),
            type_expr: None,
            new_value: Value::resource_ref("vpc".to_string(), "cidr_block".to_string(), vec![]),
        },
    ];
    let output = format_plan(
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
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name("exports_multifile");

    // Assert the multi-file load actually picked up exports.crn before
    // rendering, so the snapshot claim is backed by parsed state.
    let exported_names: Vec<&str> = fp.export_params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        exported_names,
        vec!["vpc_id", "cidr"],
        "exports.crn definitions must be merged when loading a multi-file project"
    );

    let export_changes = compute_export_diffs(&fp.export_params, &HashMap::new());
    let output = format_plan(
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
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name("exports_multifile_let_literal");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
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
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name("exports_multifile_let_chain");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
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
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name("exports_multifile_let_chain_5hop");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
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
fn plan_snapshot_exports_multifile_bare_resource_ref_seed_stays_deferred() {
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name("exports_multifile_bare_resource_ref");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
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
        !output.contains("\"${resource_binding}\"")
            && !output.contains("'${resource_binding}'")
            && !output.contains("${resource_binding}"),
        "bare structural let seed must not render as the parser placeholder string, got:\n{}",
        output
    );

    insta::assert_snapshot!(output);
}

#[test]
fn plan_snapshot_exports_multifile_use_alias_seed_does_not_leak() {
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name("exports_multifile_use_alias_seed_does_not_leak");

    let export_changes = compute_export_diffs(&fp.resolved_export_params, &HashMap::new());
    let output = strip_ansi(&format_plan(
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
        !output.contains("${use:"),
        "use-alias seed must not render the parser placeholder string, got:\n{}",
        output
    );

    insta::assert_snapshot!(output);
}

#[test]
fn plan_snapshot_exports_multifile_string_let_attr_access_rejected() {
    let fixture_path = PathBuf::from(format!(
        "{}/tests/fixtures/plan_display/exports_multifile_string_let_attr_access_rejected",
        env!("CARGO_MANIFEST_DIR")
    ));
    let mut parsed = load_configuration(&fixture_path).unwrap().parsed;
    let errors = crate::commands::validate_and_resolve_errors(&mut parsed, &fixture_path, true);
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
        message.contains("'X' is not a resource, cannot access attribute 'foo'"),
        "expected scalar field-access diagnostic naming X.foo, got:\n{}",
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
    use crate::commands::plan::ExportChange;
    use carina_core::parser::TypeExpr;
    use carina_core::resource::{ConcreteValue, Value};

    let (plan, schemas, moved_origins) = build_plan_from_fixture("no_changes");
    let export_changes = vec![
        ExportChange::Added {
            name: "new_export".to_string(),
            type_expr: Some(TypeExpr::String),
            new_value: Value::Concrete(ConcreteValue::String("hello".to_string())),
        },
        ExportChange::Modified {
            name: "changed".to_string(),
            type_expr: Some(TypeExpr::Int),
            old_json: serde_json::json!(42),
            new_value: Value::Concrete(ConcreteValue::Int(100)),
        },
        ExportChange::Removed {
            name: "obsolete".to_string(),
            old_json: serde_json::json!("gone"),
        },
    ];
    let output = format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("nested_map_diff");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("list_diff_added_struct");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("list_diff_added_struct_child_gutter");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("list_diff_modified_with_unchanged");
    let output = strip_ansi(&format_plan(
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

/// #2943: a List<String> field inside a list-of-maps modified element
/// that grew by trailing entries must render multi-line with `+` lines
/// for added entries, not as a single inline `field: [a, b, ...] →
/// [a, b, ..., c, d]` overflow line.
#[test]
fn snapshot_list_diff_string_list_grew() {
    let (plan, schemas, _moved) = build_plan_from_fixture("list_diff_string_list_grew");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("map_field_string_list_grew");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) =
        build_plan_from_fixture("list_diff_modified_with_unchanged_nested");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("list_diff_paired_all_unchanged_dropped");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("nested_list_of_maps_all_dropped");
    let output = strip_ansi(&format_plan(
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
    let (plan, schemas, _moved) = build_plan_from_fixture("list_diff_removed_struct");
    let output = strip_ansi(&format_plan(
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
    let fp = build_plan_from_fixture_name("deferred_for");
    assert!(
        !fp.deferred_for_expressions.is_empty(),
        "expected at least one deferred for-expression"
    );

    let output = strip_ansi(&format_plan(
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
    let fp = build_plan_from_fixture_name("deferred_for_create_solo");
    let delete_attributes =
        crate::fixture_plan::delete_attributes_from_plan(&fp.plan, &fp.current_states);
    let output = strip_ansi(&format_plan(
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
    let fp = build_plan_from_fixture_name("deferred_for_anonymous");
    let output = strip_ansi(&format_plan(
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
    let fp = build_plan_from_fixture_name("deferred_for_with_paired_destroy");
    let delete_attributes =
        crate::fixture_plan::delete_attributes_from_plan(&fp.plan, &fp.current_states);
    let output = strip_ansi(&format_plan(
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
    let mut fp = build_plan_from_fixture_name("deferred_for_with_dependent_wait");
    let mut plan = Plan::new();
    for effect in fp.plan.effects() {
        let mut effect = effect.clone();
        if let Effect::Create(resource) = &mut effect
            && resource.id.resource_type == "acm.CertificateValidation"
        {
            resource
                .directives
                .depends_on
                .push("validation_records".to_string());
        }
        plan.add(effect);
    }
    fp.plan = plan;
    let delete_attributes =
        crate::fixture_plan::delete_attributes_from_plan(&fp.plan, &fp.current_states);
    let output = strip_ansi(&format_plan(
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
    let fp = build_plan_from_fixture_name("deferred_for_with_unrelated_delete");
    let delete_attributes =
        crate::fixture_plan::delete_attributes_from_plan(&fp.plan, &fp.current_states);
    let output = strip_ansi(&format_plan(
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
    // Regression guard for #2426 / #2431: anonymous resource identifiers
    // gain a `<provider>_` prefix so plan output and state files
    // self-describe their provider. The header line for the lone Vpc
    // resource must read `+ awscc.ec2.Vpc awscc_ec2_vpc_<8hex>`.
    let (plan, schemas, _moved) = build_plan_from_fixture("provider_prefix");
    let output = strip_ansi(&format_plan(
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
    let fp = build_plan_from_fixture_name("server_default_struct_leaf");
    let output = strip_ansi(&format_plan(
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

/// carina#3307 + carina#3322 acceptance: when an `ExpansionTrace`
/// records two leaf resources under a composition call site, the
/// rendered plan groups them under a
/// `module "<binding>" (<source_path>)` header. The third leaf,
/// declared at the DSL root with no trace entry, stays at the top
/// level. The header label was rebadged from the internal
/// `Composition "<binding>"` term in carina#3322.
#[test]
fn snapshot_composition_folding() {
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

    plan.add(Effect::Create(inner));
    plan.add(Effect::Create(inner_role));
    plan.add(Effect::Create(logs));

    let mut trace = ExpansionTrace::new();
    // carina#3322: the call site carries the `use { source = "..." }`
    // path so the rendered header reads
    // `module "cluster" (./modules/cluster)`.
    let cluster_site = CallSite::new(
        EphemeralId::new(ResourceId::new("_virtual", "cluster")),
        "./modules/cluster",
    );
    trace.record(PersistentId::new(inner_id), vec![cluster_site.clone()]);
    trace.record(PersistentId::new(inner_role_id), vec![cluster_site]);

    let schemas = SchemaRegistry::new();
    let output = strip_ansi(&format_plan(
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

    plan.add(Effect::Create(bucket));
    plan.add(Effect::Read { resource: roles });
    plan.add(Effect::Create(cluster));

    let deferred_template = Resource::new("aws.route53.RecordSet", "validation_records");
    let deferred = DeferredForExpression {
        file: None,
        line: 1,
        header: "for option in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.RecordSet".to_string(),
        attributes: vec![(
            "name".to_string(),
            Value::resource_ref("option", "name", vec![]),
        )],
        binding_name: "validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: ForBinding::Simple("option".to_string()),
        template_resource: deferred_template,
    };
    let deferred_for_expressions = vec![deferred];

    let mut trace = ExpansionTrace::new();
    let cluster_site = CallSite::new(
        EphemeralId::new(ResourceId::new("_virtual", "cluster")),
        "./modules/cluster",
    );
    trace.record(PersistentId::new(cluster_id), vec![cluster_site]);

    let schemas = SchemaRegistry::new();
    let output = strip_ansi(&format_plan(
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
        ("+", "module", "module header"),
    ];
    for (sigil, content, label) in rows {
        let line = output
            .lines()
            .find(|line| line.contains(content))
            .unwrap_or_else(|| panic!("missing {label} row in:\n{output}"));
        let expected_prefix = format!("  {sigil} ");
        assert_eq!(
            line.find(sigil),
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
    let fp = build_plan_from_fixture_name("module_anonymous_resource");
    let output = strip_ansi(&format_plan(
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
        output.contains(r#"module "bootstrap" (./oidc-module)"#),
        "expected `module \"bootstrap\" (./oidc-module)` header from real \
         fixture in:\n{}",
        output,
    );
    assert!(
        !output.contains("Composition"),
        "internal `Composition` term must not surface to operators in:\n{}",
        output,
    );
}
