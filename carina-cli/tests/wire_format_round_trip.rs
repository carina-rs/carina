//! Wire compatibility guards for phase-typed binding addresses.

use std::collections::BTreeSet;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::UnresolvedSavedEffect;
use carina_state::{StateFile, check_and_migrate};

fn assert_json_value_round_trip<T>(fixture: &str)
where
    T: DeserializeOwned + Serialize,
{
    let original: Value = serde_json::from_str(fixture).expect("fixture must contain valid JSON");
    let decoded: T = serde_json::from_value(original.clone()).expect("fixture must deserialize");
    let reserialized = serde_json::to_value(decoded).expect("fixture must re-serialize");

    assert_eq!(reserialized, original);
}

fn assert_module_prefixed_addresses(addresses: BTreeSet<String>) {
    for expected in [
        "direct.seed",
        "direct.consumer",
        "direct.nested.leaf",
        "looped[0].seed",
        "looped[0].consumer",
        "looped[0].nested.leaf",
    ] {
        assert!(
            addresses.contains(expected),
            "decoded fixture is missing absolute address {expected:?}: {addresses:?}",
        );
    }
}

#[test]
fn pre_change_state_binding_addresses_keep_the_existing_wire_shape() {
    let fixture = include_str!("fixtures/module_binding_wire_compat/carina.state.json");
    let original: Value = serde_json::from_str(fixture).expect("fixture must contain valid JSON");
    let decoded: StateFile = check_and_migrate(fixture)
        .expect("fixture must pass the state version gate")
        .into_state();
    let addresses = decoded
        .resources()
        .iter()
        .flat_map(|resource| {
            resource
                .binding
                .iter()
                .chain(&resource.dependency_bindings)
                .chain(&resource.directives.depends_on)
        })
        .map(|address| address.as_str().to_string())
        .collect();
    let reserialized = serde_json::to_value(&decoded).expect("fixture must re-serialize");

    assert_eq!(reserialized, original);
    assert_module_prefixed_addresses(addresses);
}

#[test]
fn pre_change_saved_plan_binding_addresses_keep_the_existing_wire_shape() {
    let fixture = include_str!("fixtures/module_binding_wire_compat/carina.plan.json");
    assert_json_value_round_trip::<PlanFile>(fixture);

    let decoded: PlanFile = serde_json::from_str(fixture).expect("fixture must deserialize");
    let addresses = decoded
        .plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            UnresolvedSavedEffect::Create(resource) => Some(resource),
            _ => None,
        })
        .flat_map(|resource| {
            resource
                .binding
                .iter()
                .chain(&resource.dependency_bindings)
                .chain(&resource.directives.depends_on)
        })
        .map(|address| address.as_str().to_string())
        .collect();

    assert_module_prefixed_addresses(addresses);
}
