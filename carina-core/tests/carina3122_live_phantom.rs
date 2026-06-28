//! carina#3122 SHAPE A — reproduction from LIVE plan data.
//!
//! The fixtures in `tests/fixtures/carina3122/` are captured verbatim
//! from `aws-vault exec carina-registry-dev -- carina plan . --json`
//! against `carina-rs/infra registry/dev/registry` (2026-05-17,
//! carina 0.4.0, state_serial 33), where the plan reports a
//! never-converging `~ awscc.cloudfront.Distribution r.distribution`
//! with `(2 unchanged attributes hidden)` on an otherwise no-op run:
//!
//! - `state_distribution_config.json`   — `effects[0].Update.from.attributes.distribution_config`
//! - `desired_distribution_config.json` — `effects[0].Update.to.attributes.distribution_config`
//! - `prev_explicit.json`               — the distribution resource's
//!   stored `explicit` field from `s3://…/registry/carina.state.json`
//!   (the previously-applied authoring shape)
//!
//! Both `from` and `to` carry `distribution_config` only; `tags` is
//! byte-equal and irrelevant. The difference is entirely:
//!   1. `allowed_methods` / `cached_methods` — identical namespaced
//!      String values, element order reversed (schema:
//!      `unordered_list(Enum)`, order is NOT significant), and
//!   2. server-side read-back defaults present only in `from`
//!      (`http_version`, `ipv6_enabled`, `cache_behaviors: []`, …)
//!      that the user never authored.
//!
//! This module runs `carina_core::differ::diff` on the verbatim live
//! values with the real `prev_explicit`, isolating the projection of
//! server read-back defaults (the schema-typed `ordered:false`
//! contract for half 1 is pinned separately by the schema-passing
//! `carina3122_cloudfront_allowed_methods_*` tests in
//! `differ::comparison_tests`). See memory
//! `feedback_unit_test_path_is_not_apply_path`.
//!
//! Root cause of the original live phantom was NOT in carina-core: the
//! infra stack pinned an old awscc revision whose generated schema
//! declared `allowed_methods`/`cached_methods` as ordered lists
//! (`AttributeType::list`); awscc HEAD already emits `unordered_list`.
//! Live instrumentation confirmed the pinned WASM sent `ordered=true`
//! across the plugin boundary, so carina-core positionally compared
//! the order-reversed (but value-equal) lists and reported a change.
//! carina-core is correct given the schema it is handed; these tests
//! capture the live values and assert `NoChange`, serving as the
//! regression guard so a future stale-pin recurrence is diagnosed as
//! a provider-pin problem, not a differ bug.

use std::collections::HashMap;

use carina_core::differ::{Diff, diff};
use carina_core::explicit::ExplicitFields;
use carina_core::resource::{ConcreteValue, Resource, ResourceId, State, Value};

fn load_dc(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/carina3122/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    // The fixtures are the plan `--json` value encoding, which is the
    // tagged `Value` serde representation (`{"Map":{…}}`,
    // `{"String":"x"}`) — deserialize straight back into `Value`.
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path} as Value: {e}"))
}

fn load_explicit() -> ExplicitFields {
    let path = format!(
        "{}/tests/fixtures/carina3122/prev_explicit.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).expect("prev_explicit deserializes to ExplicitFields")
}

/// Guards the **`prev_explicit` projection** half of the live no-op:
/// the state side carries server read-back defaults the user never
/// authored (`http_version`, `ipv6_enabled`, `cache_behaviors: []`,
/// nested `forwarded_values.headers`, …); projecting through the
/// stored authoring tree must strip them so they do not produce a
/// phantom.
///
/// Schema is intentionally `None`: this isolates the projection +
/// structural-fallback behavior on the verbatim live values. It does
/// NOT pin the `ordered: false` schema-typed contract — the no-schema
/// fallback always compares lists as multisets, so the order-reversal
/// half is inert here by construction. That contract is pinned by the
/// schema-passing `carina3122_cloudfront_allowed_methods_*` tests in
/// `differ::comparison_tests`. The two together cover both halves of
/// the live phantom.
#[test]
fn carina3122_live_distribution_readback_defaults_are_projected_away() {
    let desired_dc = load_dc("desired_distribution_config.json");
    let state_dc = load_dc("state_distribution_config.json");
    let prev_explicit = load_explicit();

    let mut id = ResourceId::with_identity("cloudfront.Distribution", "r.distribution");
    id.provider = "awscc".to_string();

    let mut desired = Resource::new("cloudfront.Distribution", "r.distribution")
        .with_attribute("distribution_config", desired_dc);
    desired.id.provider = "awscc".to_string();

    let mut state_attrs: HashMap<String, Value> = HashMap::new();
    state_attrs.insert("distribution_config".to_string(), state_dc);
    let current = State::existing(id, state_attrs);

    let result = diff(&desired, &current, None, Some(&prev_explicit), None);

    assert!(
        matches!(result, Diff::NoChange(_)),
        "carina#3122: with prev_explicit projecting the state side, \
         the server read-back defaults the user never authored must \
         not produce a phantom on the verbatim live values — got \
         {result:?}"
    );
}

/// Guards that the fixtures deserialize into the tagged
/// `Value::Concrete(Map)` shape, so the projection test above cannot
/// pass for the wrong reason (e.g. both sides collapsing to some other
/// variant under a fixture-encoding regression).
#[test]
fn carina3122_fixtures_are_tagged_value_maps() {
    let desired_dc = load_dc("desired_distribution_config.json");
    let state_dc = load_dc("state_distribution_config.json");
    assert!(
        matches!(desired_dc, Value::Concrete(ConcreteValue::Map(_))),
        "desired distribution_config must be a Map, got {desired_dc:?}"
    );
    assert!(
        matches!(state_dc, Value::Concrete(ConcreteValue::Map(_))),
        "state distribution_config must be a Map, got {state_dc:?}"
    );
}
