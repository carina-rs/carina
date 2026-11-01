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
fn current_state_binding_addresses_and_resolved_dependencies_round_trip() {
    assert_eq!(StateFile::CURRENT_VERSION, 21);
    let fixture = include_str!("fixtures/module_binding_wire_compat/carina.v21.state.json");
    let original: Value = serde_json::from_str(fixture).expect("fixture must contain valid JSON");
    let decoded: StateFile = check_and_migrate(fixture)
        .expect("fixture must pass the state version gate")
        .into_state();
    let addresses = decoded
        .resources()
        .iter()
        .flat_map(|resource| resource.binding.iter())
        .map(|address| address.as_str().to_string())
        .collect();
    let reserialized = serde_json::to_value(&decoded).expect("fixture must re-serialize");

    assert_eq!(reserialized, original);
    assert_module_prefixed_addresses(addresses);
}

#[test]
fn current_saved_plan_binding_addresses_and_resolved_dependencies_round_trip() {
    let fixture = include_str!("fixtures/module_binding_wire_compat/carina.v39.plan.json");
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
        .flat_map(|resource| resource.as_resource().binding.iter())
        .map(|address| address.as_str().to_string())
        .collect();

    assert_module_prefixed_addresses(addresses);
}

#[test]
fn main_v38_saved_plans_require_regeneration_before_payload_decode() {
    for (name, fixture) in [
        (
            "carina.v38.plan.json",
            include_str!("fixtures/module_binding_wire_compat/carina.v38.plan.json"),
        ),
        (
            "carina.update-delete.v38.plan.json",
            include_str!("fixtures/module_binding_wire_compat/carina.update-delete.v38.plan.json"),
        ),
    ] {
        let wire: Value = serde_json::from_str(fixture).unwrap();
        assert_eq!(wire["version"], 38);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        std::fs::write(&path, fixture).unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .args(["apply", "--auto-approve", "--lock=false"])
            .arg(&path)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "old plan must not apply");
        assert!(
            stderr.contains("Unsupported plan file version: 38 (expected 39)")
                && stderr.contains("plan --out"),
            "version rejection must precede typed payload decoding: {stderr}"
        );
    }
}

#[test]
fn v39_plans_accept_resolved_scalar_template_directives() {
    use carina_core::resource::LoopDirective;

    assert_eq!(PlanFile::CURRENT_VERSION, 39);
    for fixture in [
        include_str!("fixtures/module_binding_wire_compat/carina.v39.plan.json"),
        include_str!("fixtures/module_binding_wire_compat/carina.update-delete.v39.plan.json"),
    ] {
        // Format 39 retains the scalar loop-directive encoding accepted by
        // main format 38 alongside complete dependency addresses.
        let mut wire: Value = serde_json::from_str(fixture).unwrap();
        wire["version"] = serde_json::json!(PlanFile::CURRENT_VERSION);
        let plan: PlanFile = serde_json::from_value(wire.clone()).unwrap();
        let mut templates = 0;
        for effect in plan.plan.effects() {
            let template = match effect {
                UnresolvedSavedEffect::DeferredCreate(payload) => &payload.template,
                UnresolvedSavedEffect::DeferredReplace(payload) => &payload.template,
                UnresolvedSavedEffect::Read { .. }
                | UnresolvedSavedEffect::Create(_)
                | UnresolvedSavedEffect::ResumeCreate { .. }
                | UnresolvedSavedEffect::Update { .. }
                | UnresolvedSavedEffect::Delete { .. }
                | UnresolvedSavedEffect::Import { .. }
                | UnresolvedSavedEffect::Remove { .. }
                | UnresolvedSavedEffect::Move { .. }
                | UnresolvedSavedEffect::ReleaseListKeys { .. }
                | UnresolvedSavedEffect::Wait { .. } => continue,
            };
            templates += 1;
            assert!(matches!(
                template.template_resource().directives().force_delete,
                LoopDirective::Resolved(_)
            ));
            assert!(matches!(
                template
                    .template_resource()
                    .directives()
                    .create_before_destroy,
                LoopDirective::Resolved(_)
            ));
        }
        assert_eq!(templates, 2);
        assert_eq!(serde_json::to_value(plan).unwrap(), wire);
    }
}

fn json_dependency_identities(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .expect("resolved-dependency slot must be a JSON array")
        .iter()
        .map(|value| {
            assert!(
                value["provider"].is_string(),
                "dependency includes provider routing: {value}"
            );
            assert!(
                value["resource_type"].is_string(),
                "dependency includes resource type: {value}"
            );
            value["identity"]
                .as_str()
                .expect("dependency includes resolved identity")
                .to_owned()
        })
        .collect()
}

#[test]
fn current_deferred_effects_keep_bound_wire_ids_and_typed_template_addresses() {
    for (fixture, variant) in [
        (
            include_str!("fixtures/module_binding_wire_compat/carina.v39.plan.json"),
            "DeferredCreate",
        ),
        (
            include_str!("fixtures/module_binding_wire_compat/carina.update-delete.v39.plan.json"),
            "DeferredReplace",
        ),
    ] {
        let round_tripped = assert_json_value_round_trip::<PlanFile>(fixture);
        assert_eq!(round_tripped["version"], 39);
        let deferred = round_tripped["plan"]["effects"]
            .as_array()
            .expect("saved effects")
            .iter()
            .filter_map(|effect| effect.get(variant))
            .collect::<Vec<_>>();
        assert_eq!(deferred.len(), 2, "fixture must exercise {variant}");
        let mut bindings = BTreeSet::new();
        for effect in deferred {
            let binding = effect["template"]["target"]["binding"]
                .as_str()
                .expect("bound loop");
            bindings.insert(binding.to_owned());
            assert_eq!(
                effect["id"]["identity"], binding,
                "bound wire identity remains the bound spelling"
            );
            assert_eq!(
                effect["template"]["template_resource"]["id"]["identity"],
                format!("{binding}[?]")
            );
            assert_eq!(
                effect["template"]["template_resource"]["binding"],
                format!("{binding}[?]")
            );
        }
        assert_eq!(
            bindings,
            BTreeSet::from([
                "direct.children".to_owned(),
                "looped[0].children".to_owned()
            ])
        );
    }
}

#[test]
fn current_update_delete_plan_keeps_prefixed_wait_and_dependency_slots() {
    let fixture =
        include_str!("fixtures/module_binding_wire_compat/carina.update-delete.v39.plan.json");
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
        let identity = wait["id"]["identity"].as_str().expect("wait identity");
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
            json_dependency_identities(&wait["explicit_dependencies"]),
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
            json_dependency_identities(&update["from"]["dependency_bindings"]),
            BTreeSet::from([format!("{prefix}.seed"), "root".to_string()]),
            "Update.from dependency bindings must retain absolute spellings",
        );
        assert_eq!(
            json_dependency_identities(&update["to"]["explicit_dependencies"]),
            BTreeSet::from([format!("{prefix}.seed"), "root".to_string()]),
            "Update.to directive dependencies must retain absolute spellings",
        );
    }

    let deletes = effects
        .iter()
        .filter_map(|effect| effect.get("Delete"))
        .collect::<Vec<_>>();
    assert_eq!(
        deletes.len(),
        4,
        "nested removals and deferred-source replacements"
    );
    let nested_deletes = deletes
        .iter()
        .copied()
        .filter(|delete| {
            delete["binding"]
                .as_str()
                .expect("delete binding")
                .ends_with(".nested.leaf")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        nested_deletes.len(),
        2,
        "fixture must retain both nested Delete effects"
    );
    let source_deletes = deletes
        .iter()
        .copied()
        .filter(|delete| {
            delete["binding"]
                .as_str()
                .expect("delete binding")
                .ends_with(".deferred_source")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        source_deletes.len(),
        2,
        "both deferred sources must be replaced"
    );
    for delete in source_deletes {
        assert_eq!(delete["id"]["identity"], delete["binding"]);
        assert!(json_dependency_identities(&delete["dependencies"]).is_empty());
        assert!(json_dependency_identities(&delete["explicit_dependencies"]).is_empty());
    }
    for delete in nested_deletes {
        let binding = delete["binding"].as_str().expect("delete binding");
        let prefix = binding
            .strip_suffix(".nested.leaf")
            .expect("module-prefixed nested delete");
        let expected = BTreeSet::from([format!("{prefix}.consumer")]);
        assert_eq!(
            json_dependency_identities(&delete["dependencies"]),
            expected,
            "Delete dependencies must retain absolute spellings",
        );
        assert_eq!(
            json_dependency_identities(&delete["explicit_dependencies"]),
            expected,
            "Delete explicit dependencies must retain absolute spellings",
        );
        assert!(
            delete["directives"].get("depends_on").is_none(),
            "authored dependency operands cannot reappear on a resolved delete"
        );
    }
}
