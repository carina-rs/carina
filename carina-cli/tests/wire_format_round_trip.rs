//! Wire compatibility guards for phase-typed binding addresses.

use std::collections::BTreeSet;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::UnresolvedSavedEffect;
use carina_state::{StateFile, check_and_migrate};

fn assert_json_value_round_trip<T>(fixture: &str) -> Value
where
    T: DeserializeOwned + Serialize,
{
    let original: Value = serde_json::from_str(fixture).expect("fixture must contain valid JSON");
    let decoded: T = serde_json::from_value(original.clone()).expect("fixture must deserialize");
    let reserialized = serde_json::to_value(decoded).expect("fixture must re-serialize");

    assert_eq!(reserialized, original);
    reserialized
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

fn json_strings(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .expect("binding-address slot must be a JSON array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("binding address must stay a JSON string")
                .to_string()
        })
        .collect()
}

#[test]
fn base_binary_update_delete_plan_keeps_prefixed_wait_and_dependency_slots() {
    let fixture =
        include_str!("fixtures/module_binding_wire_compat/carina.update-delete.plan.json");
    let round_tripped = assert_json_value_round_trip::<PlanFile>(fixture);

    let wait_bindings = round_tripped["wait_bindings"]
        .as_array()
        .expect("saved plan must contain wait bindings")
        .iter()
        .map(|wait| {
            (
                wait["binding"].as_str().expect("wait binding").to_string(),
                wait["target"].as_str().expect("wait target").to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        wait_bindings,
        BTreeSet::from([
            (
                "direct.consumer_ready".to_string(),
                "direct.consumer".to_string(),
            ),
            (
                "looped[0].consumer_ready".to_string(),
                "looped[0].consumer".to_string(),
            ),
        ]),
    );

    let effects = round_tripped["plan"]["effects"]
        .as_array()
        .expect("saved plan effects");
    let read_bindings = effects
        .iter()
        .filter_map(|effect| effect.get("Read"))
        .map(|read| {
            read["resource"]["binding"]
                .as_str()
                .expect("data-source binding")
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        read_bindings,
        BTreeSet::from(["direct.lookup".to_string(), "looped[0].lookup".to_string(),]),
    );

    let waits = effects
        .iter()
        .filter_map(|effect| effect.get("Wait"))
        .collect::<Vec<_>>();
    assert_eq!(waits.len(), 2, "fixture must exercise Wait effects");
    for wait in waits {
        let identity = wait["identity"].as_str().expect("wait identity");
        let prefix = identity
            .strip_suffix(".consumer_ready")
            .expect("module-prefixed wait identity");
        let expected_target = format!("{prefix}.consumer");
        assert_eq!(
            wait["target_id"]["identity"].as_str(),
            Some(expected_target.as_str()),
            "Wait target must retain its absolute spelling",
        );
        assert_eq!(
            json_strings(&wait["explicit_dependencies"]),
            BTreeSet::from([format!("{prefix}.seed")]),
            "Wait dependencies must retain absolute spellings",
        );
    }

    let updates = effects
        .iter()
        .filter_map(|effect| effect.get("Update"))
        .collect::<Vec<_>>();
    assert_eq!(updates.len(), 2, "fixture must exercise Update effects");
    for update in updates {
        let binding = update["to"]["binding"].as_str().expect("update binding");
        let prefix = binding
            .strip_suffix(".consumer")
            .expect("module-prefixed consumer update");
        assert_eq!(
            json_strings(&update["from"]["dependency_bindings"]),
            BTreeSet::from([format!("{prefix}.seed"), "root".to_string()]),
            "Update.from dependency bindings must retain absolute spellings",
        );
        assert_eq!(
            json_strings(&update["to"]["directives"]["depends_on"]),
            BTreeSet::from([format!("{prefix}.seed"), "root".to_string()]),
            "Update.to directive dependencies must retain absolute spellings",
        );
    }

    let deletes = effects
        .iter()
        .filter_map(|effect| effect.get("Delete"))
        .collect::<Vec<_>>();
    assert_eq!(deletes.len(), 2, "fixture must exercise Delete effects");
    for delete in deletes {
        let binding = delete["binding"].as_str().expect("delete binding");
        let prefix = binding
            .strip_suffix(".nested.leaf")
            .expect("module-prefixed nested delete");
        let expected = BTreeSet::from([format!("{prefix}.consumer")]);
        assert_eq!(
            json_strings(&delete["dependencies"]),
            expected,
            "Delete dependencies must retain absolute spellings",
        );
        assert_eq!(
            json_strings(&delete["explicit_dependencies"]),
            expected,
            "Delete explicit dependencies must retain absolute spellings",
        );
        assert_eq!(
            json_strings(&delete["directives"]["depends_on"]),
            expected,
            "Delete directive dependencies must retain absolute spellings",
        );
    }
}
