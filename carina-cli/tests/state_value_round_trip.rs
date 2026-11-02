//! Phase 1 of RFC #2972 — `Value` codec round-trip guard.
//!
//! State files persist resource attributes with the typed persisted-value
//! codec, which lifts them into the rich `Value` enum and lowers them through
//! `PersistedValue`. When Phase 5 physically
//! restructures `Value` from the flat 14-variant shape into nested
//! `Concrete(ConcreteValue) / Deferred(DeferredValue)`, both codec
//! halves must keep producing/accepting **the same JSON shape** —
//! end users have on-disk state files in the legacy layout and
//! cannot be asked to migrate.
//!
//! This test exercises every `carina.state.json` fixture in the repo,
//! pulls each `attributes` JSON value through
//! `PersistedAttributes::to_values -> PersistedValue::encode_provider_value -> decode` and
//! asserts the second pass equals the first via `Value::PartialEq`. Typed
//! `RedactedSecret` values are included because this is the resource-state
//! codec and must preserve already-protected state without rehashing it.
//! Today (Phase 1, additive only) this passes trivially; the test
//! fails the moment a Phase-5 codec change re-shapes the JSON
//! produced or interpreted by the persisted codec.

use std::path::{Path, PathBuf};

use carina_core::value::PersistedValue;
use carina_state::check_and_migrate;

fn find_state_fixtures() -> Vec<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let root = PathBuf::from(manifest_dir).join("tests/fixtures");
    let mut out = Vec::new();
    walk(&root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else if name == "carina.state.json" {
            out.push(path);
        }
    }
}

#[test]
fn fixture_attributes_round_trip_through_value_codec() {
    let fixtures = find_state_fixtures();
    assert!(
        !fixtures.is_empty(),
        "no carina.state.json fixtures found under tests/fixtures — test is a no-op",
    );

    let mut total_attrs = 0_usize;
    let mut failures: Vec<String> = Vec::new();

    for path in &fixtures {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let state = match check_and_migrate(&raw) {
            Ok(s) => s.into_state(),
            Err(e) => {
                failures.push(format!("{}: deserialize StateFile: {e}", path.display()));
                continue;
            }
        };

        for resource in state.resources() {
            let resource_id = format!(
                "{}.{}.{}",
                resource.provider, resource.resource_type, resource.identity,
            );
            let persisted = resource
                .persisted_attributes()
                .expect("fixture resource has persisted attributes");
            total_attrs += persisted.keys().count();
            for (attr_name, value_pass1) in persisted.to_values() {
                // Lower: Value → JSON via the production codec.
                // Some inputs (e.g. a hypothetical `Value::Deferred(DeferredValue::Secret)` or
                // `Value::Deferred(DeferredValue::Unknown)` in a fixture) intentionally error
                // here. None of the in-tree fixtures contain such
                // values, but if a future fixture adds one, surface
                // the error rather than swallow it.
                let persisted_pass1 =
                    match PersistedValue::encode_provider_value(&value_pass1, None) {
                        Ok(j) => j,
                        Err(e) => {
                            failures.push(format!(
                                "{} attr `{}` ({}): persisted encode error: {e}",
                                path.display(),
                                attr_name,
                                resource_id,
                            ));
                            continue;
                        }
                    };

                // Second lift: re-parse the just-emitted JSON.
                let Some(value_pass2) = persisted_pass1.decode() else {
                    failures.push(format!(
                        "{} attr `{}` ({}): pass-2 persisted decode returned None \
                         (codec emitted JSON null for a non-null Value)",
                        path.display(),
                        attr_name,
                        resource_id,
                    ));
                    continue;
                };

                // The two `Value`s must be `PartialEq`-equal. This is
                // the load-bearing assertion: a Phase-5 codec change
                // that re-shapes the JSON layer would surface here as
                // `value_pass1 != value_pass2`.
                if value_pass1 != value_pass2 {
                    failures.push(format!(
                        "{} attr `{}` ({}): codec round-trip not idempotent\n  \
                         pass1: {value_pass1:?}\n  pass2: {value_pass2:?}",
                        path.display(),
                        attr_name,
                        resource_id,
                    ));
                }
            }
        }
    }

    assert!(total_attrs > 0, "round-trip exercised zero attributes");
    assert!(
        failures.is_empty(),
        "{} attribute(s) failed Value codec round-trip across {} fixture(s):\n{}",
        failures.len(),
        fixtures.len(),
        failures.join("\n"),
    );
}

#[test]
fn current_state_root_and_nested_hashes_round_trip_byte_for_byte() {
    use carina_core::resource::{ConcreteValue, DeferredValue, SnapshotLeaf, Value};
    use carina_state::StateFile;
    assert_eq!(StateFile::CURRENT_VERSION, 21);
    let fixture = include_str!("fixtures/module_binding_wire_compat/carina.v21.state.json");
    let mut wire: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let attributes = wire["resources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find_map(|row| {
            let attrs = row["current"]["attributes"].as_object_mut()?;
            attrs.get("comment")?.get("$secret")?;
            Some(attrs)
        })
        .expect("current fixture contains a root hash");
    let root = attributes["comment"].clone();
    attributes.insert(
        "nested_secret".into(),
        serde_json::json!({
            "plain": "still selectable", "list": [{ "token": root }]
        }),
    );
    let state = check_and_migrate(&serde_json::to_string(&wire).unwrap())
        .expect("current state fixture passes the production version gate")
        .into_state();
    let bytes = carina_core::utils::pretty_with_newline_bytes(&state).unwrap();
    let reloaded = check_and_migrate(std::str::from_utf8(&bytes).unwrap())
        .unwrap()
        .into_state();
    assert_eq!(
        carina_core::utils::pretty_with_newline_bytes(&reloaded).unwrap(),
        bytes
    );
    let row = reloaded
        .resources()
        .iter()
        .find(|row| {
            row.persisted_attributes()
                .is_some_and(|attrs| attrs.contains_key("nested_secret"))
        })
        .unwrap();
    let attrs = row.persisted_attributes().unwrap();
    assert!(matches!(
        attrs.get("comment"),
        Some(Value::Deferred(DeferredValue::Placeholder(
            SnapshotLeaf::RedactedSecret { .. }
        )))
    ));
    let nested = attrs.get("nested_secret").unwrap();
    assert!(matches!(nested, Value::Concrete(ConcreteValue::Map(_))));
    assert_eq!(
        nested
            .to_value_at("state.nested_secret")
            .unwrap_err()
            .path(),
        "state.nested_secret.list[0].token"
    );
    let Value::Concrete(ConcreteValue::Map(fields)) = nested else {
        unreachable!()
    };
    assert_eq!(
        fields["plain"]
            .to_value_at("state.nested_secret.plain")
            .unwrap(),
        Value::Concrete(ConcreteValue::String("still selectable".into()))
    );
}
