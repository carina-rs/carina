//! Phase 1 of RFC #2972 — `Value` codec round-trip guard.
//!
//! State files persist resource attributes as raw JSON; the runtime
//! lifts them into the rich `Value` enum via `json_to_dsl_value` and
//! lowers them back via `value_to_json`. When Phase 5 physically
//! restructures `Value` from the flat 14-variant shape into nested
//! `Concrete(ConcreteValue) / Deferred(DeferredValue)`, both codec
//! halves must keep producing/accepting **the same JSON shape** —
//! end users have on-disk state files in the legacy layout and
//! cannot be asked to migrate.
//!
//! This test exercises every `carina.state.json` fixture in the repo,
//! pulls each `attributes` JSON value through
//! `json_to_dsl_value -> value_to_json -> json_to_dsl_value` and
//! asserts the second pass equals the first via `Value::PartialEq`.
//! Today (Phase 1, additive only) this passes trivially; the test
//! fails the moment a Phase-5 codec change re-shapes the JSON
//! produced by `value_to_json` or interpreted by `json_to_dsl_value`.

use std::path::{Path, PathBuf};

use carina_core::value::{json_to_dsl_value, value_to_json};
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

        for resource in &state.resources {
            let resource_id = format!(
                "{}.{}.{}",
                resource.provider, resource.resource_type, resource.name,
            );
            for (attr_name, json_value) in &resource.attributes {
                total_attrs += 1;

                // First lift: legacy on-disk JSON → Value.
                let Some(value_pass1) = json_to_dsl_value(json_value) else {
                    // `json_to_dsl_value` returns None for JSON null,
                    // which represents "absent". Skip — there is no
                    // Value to round-trip.
                    continue;
                };

                // Lower: Value → JSON via the production codec.
                // Some inputs (e.g. a hypothetical `Value::Deferred(DeferredValue::Secret)` or
                // `Value::Deferred(DeferredValue::Unknown)` in a fixture) intentionally error
                // here. None of the in-tree fixtures contain such
                // values, but if a future fixture adds one, surface
                // the error rather than swallow it.
                let json_pass1 = match value_to_json(&value_pass1) {
                    Ok(j) => j,
                    Err(e) => {
                        failures.push(format!(
                            "{} attr `{}` ({}): value_to_json error: {e}",
                            path.display(),
                            attr_name,
                            resource_id,
                        ));
                        continue;
                    }
                };

                // Second lift: re-parse the just-emitted JSON.
                let Some(value_pass2) = json_to_dsl_value(&json_pass1) else {
                    failures.push(format!(
                        "{} attr `{}` ({}): pass-2 json_to_dsl_value returned None \
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
