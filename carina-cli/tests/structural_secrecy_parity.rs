//! Failing-first #4470 acceptance tests through today's CLI binary.
//! Limit messages are shared with core; validation needs no backend writes or apply.

#[path = "support/structural_secrecy.rs"]
mod fixtures;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Output};

use carina_core::paths::CanonicalPath;
use fixtures::{Finding, Site};
use serde_json::{Value, json};

fn validate(root: &Path, json: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(root)
        .args(["validate", "."])
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    if json {
        command.arg("--json");
    }
    command.output().expect("run carina validate")
}

fn output_text(output: &Output) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !text.contains(fixtures::CANARY),
        "validation output disclosed a secret payload"
    );
    text
}

fn source_file(root: &Path, label: &str) -> CanonicalPath {
    let label = Path::new(label);
    let path = if label.is_absolute() {
        label.to_path_buf()
    } else {
        root.join(label)
    };
    CanonicalPath::new(path).expect("diagnostic names an existing file")
}

fn at_site(root: &Path, location: &Value, site: Site) -> bool {
    let Some(file) = location["file"].as_str() else {
        return false;
    };
    source_file(root, file) == source_file(root, site.file)
        && location["range"]["start"] == json!({"line": site.line, "column": site.column})
        && site.end.is_none_or(|(line, column)| {
            location["range"]["end"] == json!({"line": line, "column": column})
        })
}

fn assert_validation(root: &Path, expected: &[Finding]) -> (Value, String) {
    // Collect both modes before asserting so each failing-first test exercises
    // text and JSON even when today's CLI incorrectly accepts the configuration.
    let human = validate(root, false);
    let machine = validate(root, true);
    let human_text = output_text(&human);
    let machine_text = output_text(&machine);
    let exit_code = if expected.is_empty() { 0 } else { 1 };
    assert_eq!(human.status.code(), Some(exit_code), "{human_text}");
    assert_eq!(machine.status.code(), Some(exit_code), "{machine_text}");
    assert!(machine.stderr.is_empty(), "{machine_text}");
    let report: Value = serde_json::from_slice(&machine.stdout).expect("one JSON report");
    assert_eq!(
        report["status"],
        if expected.is_empty() { "ok" } else { "error" }
    );
    let errors = report["errors"].as_array().cloned().unwrap_or_default();
    assert_eq!(errors.len(), expected.len(), "{report}");
    // Deep chains report thousands of attributes. Index once instead of
    // canonicalizing every actual path again for every expected finding.
    let mut files = HashMap::new();
    let mut by_site = HashMap::<_, Vec<_>>::new();
    for error in &errors {
        let label = error["location"]["file"].as_str().expect("source file");
        let file = files
            .entry(label)
            .or_insert_with(|| source_file(root, label));
        by_site
            .entry((
                file.clone(),
                error["location"]["range"]["start"]["line"]
                    .as_u64()
                    .unwrap(),
                error["location"]["range"]["start"]["column"]
                    .as_u64()
                    .unwrap(),
                error["message"].as_str().unwrap(),
            ))
            .or_default()
            .push(error);
    }
    let expected_files = expected
        .iter()
        .map(|finding| finding.site.file)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|file| (file, source_file(root, file)))
        .collect::<BTreeMap<_, _>>();
    let human_sites = human_text
        .lines()
        .flat_map(|line| {
            expected_files
                .keys()
                .filter_map(move |file| line.find(*file).map(|start| &line[start..]))
        })
        .collect::<HashSet<_>>();
    for finding in expected {
        let matching = by_site
            .get(&(
                expected_files[finding.site.file].clone(),
                u64::from(finding.site.line),
                u64::from(finding.site.column),
                finding.message,
            ))
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert_eq!(
            matching.len(),
            1,
            "missing or duplicated {finding:?}: {report}"
        );
        if let Some((line, column)) = finding.site.end {
            assert_eq!(
                matching[0]["location"]["range"]["end"],
                json!({"line": line, "column": column}),
                "{report}"
            );
        }
        assert_eq!(matching[0]["severity"], "error", "{report}");
        // Text must pair the same message and authored file:line:column on one
        // diagnostic line; an unrelated location elsewhere is insufficient.
        let label = format!(
            "{}:{}:{}",
            finding.site.file, finding.site.line, finding.site.column
        );
        assert!(
            human_sites.contains(format!("{label}: {}", finding.message).as_str())
                || (expected.len() < 128
                    && human_text
                        .lines()
                        .any(|line| line.contains(&label) && line.contains(finding.message))),
            "missing {label}: {} in {human_text}",
            finding.message
        );
    }
    if expected.is_empty() {
        for message in [
            fixtures::COALESCE_MESSAGE,
            fixtures::LOOKUP_MESSAGE,
            fixtures::DEPTH_LIMIT_MESSAGE.as_str(),
            fixtures::VISITS_LIMIT_MESSAGE.as_str(),
        ] {
            assert!(!human_text.contains(message), "{human_text}");
            assert!(!machine_text.contains(message), "{machine_text}");
        }
    } else {
        assert_eq!(report["resource_count"], 0);
        assert_eq!(report["resources"], json!([]));
    }
    (report, human_text)
}

#[test]
fn mixed_coalesce_rejects_both_directions_pending_selected_and_wrapped() {
    let fixture = fixtures::fixture("mixed_coalesce");
    assert_validation(fixture.path(), fixtures::COALESCE_FINDINGS);
}

#[test]
fn mixed_alternatives_do_not_hide_later_default_tag_diagnostics_or_duplicate_placements() {
    let fixture = fixtures::mixed_and_default_tag_fixture();
    for json in [false, true] {
        let output = validate(fixture.path(), json);
        let text = output_text(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(text.contains(fixtures::COALESCE_MESSAGE), "{text}");
        assert!(text.contains("default_tags.Owner"), "{text}");
        if json {
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            let errors = report["errors"].as_array().unwrap();
            assert_eq!(errors.len(), 2, "{report}");
            assert_eq!(
                errors
                    .iter()
                    .filter(|error| error["message"] == fixtures::COALESCE_MESSAGE)
                    .count(),
                1,
                "{report}"
            );
            let distinct = errors
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<HashSet<_>, _>>()
                .unwrap();
            assert_eq!(
                distinct.len(),
                errors.len(),
                "placements must be included only once: {report}"
            );
        }
    }
}

#[test]
fn mixed_lookup_rejects_known_pending_unused_defaults_and_public_upstream_map() {
    let fixture = fixtures::fixture("mixed_lookup");
    assert_validation(fixture.path(), fixtures::LOOKUP_FINDINGS);
    assert!(
        !fixture.path().join("up/carina.state.json").exists(),
        "validation must use upstream declarations without backend I/O"
    );
}

#[test]
fn homogeneous_coalesce_lookup_and_if_remain_valid() {
    let fixture = fixtures::fixture("homogeneous");
    assert_validation(fixture.path(), &[]);
}

fn contains_location(value: &Value, root: &Path, site: Site) -> bool {
    // Call-site metadata may be nested under a related/context field. Its
    // spelling is not part of D2, but it must retain a structured source site.
    match value {
        Value::Object(fields) => {
            (fields.contains_key("file")
                && fields.contains_key("range")
                && at_site(root, value, site))
                || fields
                    .values()
                    .any(|child| contains_location(child, root, site))
        }
        Value::Array(children) => children
            .iter()
            .any(|child| contains_location(child, root, site)),
        _ => false,
    }
}

fn assert_module_validation(root: &Path, finding: Finding) {
    let (report, text) = assert_validation(root, &[finding]);
    let error = &report["errors"][0];
    assert!(
        contains_location(error, root, fixtures::SECRET_CALL),
        "the authored operator also needs the secret instance's call site: {error}"
    );
    assert!(text.contains("secret_instance"), "{text}");
    assert!(
        !text
            .lines()
            .any(|line| line.contains("public_instance")
                && line.contains(fixtures::COALESCE_MESSAGE)),
        "{text}"
    );
}

#[test]
fn module_instances_blame_authored_operator_and_identify_only_secret_call() {
    let fixture = fixtures::fixture("module_instances");
    assert_module_validation(fixture.path(), fixtures::MODULE_FINDING);
}

#[test]
fn root_failure_does_not_hide_the_module_instance_finding() {
    let fixture = fixtures::root_and_module_fixture();
    assert_validation(fixture.path(), fixtures::ROOT_AND_MODULE_FINDINGS);
}

#[test]
fn impossible_expansion_keeps_the_same_root_secrecy_subset_as_lsp() {
    let fixture = fixtures::unexpandable_root_and_module_fixture();
    let output = validate(fixture.path(), true);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    let findings = report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|error| {
            error["message"]
                .as_str()
                .is_some_and(fixtures::is_secrecy_message)
        })
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 1, "{report}");
    assert!(at_site(
        fixture.path(),
        &findings[0]["location"],
        fixtures::ROOT_AND_MODULE_FINDINGS[1].site
    ));
}

#[test]
fn ordinary_root_errors_still_collect_expanded_secrecy_findings() {
    let fixture = fixtures::root_and_module_with_backend_error_fixture();
    let output = validate(fixture.path(), true);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    let findings = report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|error| {
            error["message"]
                .as_str()
                .is_some_and(fixtures::is_secrecy_message)
        })
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 2, "{report}");
    assert!(
        report["errors"].as_array().unwrap().len() > findings.len(),
        "{report}"
    );
    for expected in fixtures::ROOT_AND_MODULE_FINDINGS {
        assert!(
            findings.iter().any(|finding| at_site(
                fixture.path(),
                &finding["location"],
                expected.site
            )),
            "{report}"
        );
    }
}

#[test]
fn failed_validation_reuses_expansion_placements_without_duplicates() {
    let fixture = fixtures::root_and_module_with_backend_error_fixture();
    let path = fixture.path().join("main.crn");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\nlet converted = 'ok' as String\nlet conversion_sink = mock.test.resource { name = converted }\n");
    std::fs::write(path, text).unwrap();
    let output = validate(fixture.path(), true);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(
        errors
            .iter()
            .filter(|error| error["message"]
                .as_str()
                .is_some_and(|message| message
                    .contains("conversion target must be a loaded named scalar type")))
            .count(),
        1,
        "{report}"
    );
    assert_eq!(
        errors
            .iter()
            .filter(|error| error["message"]
                .as_str()
                .is_some_and(fixtures::is_secrecy_message))
            .count(),
        2,
        "{report}"
    );
    assert_eq!(errors.len(), 4, "{report}");
    assert_eq!(
        errors
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<HashSet<_>, _>>()
            .unwrap()
            .len(),
        errors.len(),
        "{report}"
    );
}

#[test]
fn non_attribute_declarations_use_their_authored_kind_and_name() {
    let (fixture, findings) = fixtures::declaration_kinds_fixture();
    assert_validation(fixture.path(), &findings);
}

#[test]
fn module_dependent_if_preserves_instance_secrecy_and_authored_operator() {
    let fixture = fixtures::module_if_fixture();
    assert_module_validation(fixture.path(), fixtures::MODULE_IF_FINDING);
}

#[test]
fn public_module_instance_remains_valid() {
    let fixture = fixtures::public_module_fixture();
    assert_validation(fixture.path(), &[]);
}

#[test]
fn reference_depth_limit_rejects_r128_after_shallow_and_deep_cache_roots() {
    for shape in fixtures::CHAIN_SHAPES {
        for shallow_first in [true, false] {
            let (fixture, findings) = fixtures::depth_fixture(128, shallow_first, shape);
            assert_validation(fixture.path(), &findings);
        }
    }
}

#[test]
fn reference_depth_limit_accepts_r127_after_shallow_and_deep_cache_roots() {
    for shape in fixtures::CHAIN_SHAPES {
        for shallow_first in [true, false] {
            let (fixture, _) = fixtures::depth_fixture(127, shallow_first, shape);
            assert_validation(fixture.path(), &[]);
        }
    }
}

#[test]
fn expression_wrapped_chains_accept_127_and_reject_128_in_both_shapes_and_orders() {
    for shape in fixtures::CHAIN_SHAPES {
        for wrapper in fixtures::EXPRESSION_WRAPPERS {
            for shallow_first in [true, false] {
                for last in [127, 128] {
                    let (fixture, findings) =
                        fixtures::expression_depth_fixture(last, shallow_first, shape, wrapper);
                    assert_validation(fixture.path(), &findings);
                }
            }
        }
    }
}

#[test]
fn module_output_chains_accept_63_and_reject_64_in_both_shapes() {
    for shape in fixtures::CHAIN_SHAPES {
        for instances in [63, 64] {
            let (fixture, findings) = fixtures::module_output_depth_fixture(instances, shape);
            assert_validation(fixture.path(), &findings);
        }
    }
}

#[test]
fn one_reference_in_deep_expressions_is_valid_in_both_shapes() {
    for shape in fixtures::CHAIN_SHAPES {
        for (wrapper, levels) in [
            (fixtures::ExpressionWrapper::Builtin, 130),
            (fixtures::ExpressionWrapper::List, 70),
        ] {
            let fixture = fixtures::deep_expression_fixture(shape, wrapper, levels);
            assert_validation(fixture.path(), &[]);
        }
    }
}

#[test]
fn issue_4474_deep_chains_return_limit_errors_without_crashing() {
    for shape in fixtures::CHAIN_SHAPES {
        for last in [256, 5000] {
            let (fixture, findings) = fixtures::depth_fixture(last, false, shape);
            assert_validation(fixture.path(), &findings);
        }
    }
}

#[test]
fn reference_visit_limit_rejects_4097_references_including_cache_hits() {
    for shape in fixtures::CHAIN_SHAPES {
        let (fixture, finding) = fixtures::visits_fixture(4097, shape);
        assert_validation(fixture.path(), &[finding]);
    }
}

#[test]
fn reference_visit_limit_accepts_4096_references_and_resets_per_attribute() {
    for shape in fixtures::CHAIN_SHAPES {
        let (fixture, _) = fixtures::visits_fixture(4096, shape);
        assert_validation(fixture.path(), &[]);
    }
}

#[test]
fn shared_fan_out_dag_is_valid_and_does_not_look_like_a_cycle() {
    let fixture = fixtures::fan_out_fixture();
    assert_validation(fixture.path(), &[]);
}
