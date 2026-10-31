//! Wire compatibility guards for phase-typed binding addresses.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use carina_cli::commands::plan::PlanFile;
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

#[test]
fn committed_state_binding_addresses_keep_the_existing_wire_shape() {
    let fixture =
        include_str!("fixtures/plan_display/moved_hint_read_loop_module/carina.state.json");
    let original: Value = serde_json::from_str(fixture).expect("fixture must contain valid JSON");
    let decoded: StateFile = check_and_migrate(fixture)
        .expect("fixture must pass the state version gate")
        .into_state();
    let reserialized = serde_json::to_value(decoded).expect("fixture must re-serialize");

    assert_eq!(reserialized, original);
}

#[test]
fn committed_saved_plan_binding_addresses_keep_the_existing_wire_shape() {
    assert_json_value_round_trip::<PlanFile>(include_str!(
        "fixtures/deferred_unavailable_saved_plan/carina.plan.json"
    ));
}
