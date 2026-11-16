//! Full-corpus and per-invocation work observations through production entries.
use carina_core::test_utils::resolver_kind::{
    EVENTS, ProjectionReport, STORAGE_EVENTS, compare_directory,
};
use carina_core::{
    EnvironmentSnapshot, parser::ProviderContext, paths::ResolvedPath, schema::SchemaRegistry,
};
use serde_json::Value as Json;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}
fn directories(root: &Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            directories(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "crn") {
            out.insert(
                path.parent()
                    .unwrap()
                    .strip_prefix(repo())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}
fn observe(path: &Path) -> ProjectionReport {
    let mut schemas = SchemaRegistry::new();
    for schema in carina_provider_mock::builtin_schemas() {
        schemas.insert("mock", schema);
    }
    let context = ProviderContext {
        resource_types: ProviderContext::resource_types_from_schema_registry(&schemas),
        ..ProviderContext::new(EnvironmentSnapshot::empty())
    };
    compare_directory(&ResolvedPath::new(path).unwrap(), &context, &schemas)
}
fn check(report: &ProjectionReport, label: &str) {
    assert_eq!(
        report.strict_succeeded, report.plain_strict_succeeded,
        "{label}: strict success differs from the independent plain run"
    );
    assert_eq!(
        report.diagnostics_succeeded, report.plain_diagnostics_succeeded,
        "{label}: diagnostics success differs from the independent plain run"
    );
    let actual_publications = report.publications();
    assert_eq!(
        actual_publications.len(),
        report.plain_publications.len(),
        "{label}: published-root count differs"
    );
    assert_eq!(
        actual_publications, report.plain_publications,
        "{label}: published-root identities/order differ"
    );
    for invocation in &report.invocations {
        assert!(
            invocation.same_inputs && invocation.same_roots && invocation.same_error,
            "{label}: invocation {} inputs, roots or error differ",
            invocation.ordinal
        );
    }

    assert!(
        report.strict_error_matches,
        "{label}: strict directory error differs"
    );
    assert!(
        report.diagnostics_error_matches,
        "{label}: diagnostics directory error differs"
    );
    for root in &report.roots {
        assert!(
            root.matches_plain,
            "{label}: projection differs in {} invocation {}",
            root.entry, root.invocation
        );
        assert_eq!(
            root.counts, root.oracle_counts,
            "{label}: root work differs in {} invocation {}",
            root.entry, root.invocation
        );
        assert!(
            report
                .invocations
                .iter()
                .any(|invocation| invocation.ordinal == root.invocation
                    && invocation.kind == root.invocation_kind
                    && invocation.entry == root.entry),
            "{label}: root is missing its tagged invocation"
        );
    }
    for (index, invocation) in report.invocations.iter().enumerate() {
        assert_eq!(invocation.ordinal, index + 1, "{label}: invocation ordinal");
        assert!(
            invocation.same_inputs,
            "{label}: original inputs/bindings differ in {} invocation {}",
            invocation.entry, invocation.ordinal
        );
        assert!(
            invocation.same_roots,
            "{label}: root count differs in {} invocation {}",
            invocation.entry, invocation.ordinal
        );
        assert!(
            invocation.same_error,
            "{label}: invocation error differs in {} invocation {}",
            invocation.entry, invocation.ordinal
        );
        assert_eq!(
            invocation.counts, invocation.oracle_counts,
            "{label}: resolution work differs in {} invocation {}",
            invocation.entry, invocation.ordinal
        );
    }
}
#[test]
fn all_fixture_directories_project_identically() {
    let mut discovered = BTreeSet::new();
    let mut contributions = Vec::new();
    for root in [
        "carina-cli/tests/fixtures/plan_display",
        "carina-core/tests/fixtures/diagnostic_order",
    ] {
        let before = discovered.len();
        // A nested directory with a .crn file is also loaded as a root.
        directories(&repo().join(root), &mut discovered);
        contributions.push(discovered.len() - before);
    }
    assert!(
        !discovered.is_empty() && contributions.iter().all(|count| *count > 0),
        "empty fixture walk: {} directories; root contributions {contributions:?}",
        discovered.len()
    );
    let reports = discovered
        .iter()
        .map(|label| (label, observe(&repo().join(label))))
        .collect::<Vec<_>>();
    let invocations = reports
        .iter()
        .map(|(_, report)| report.invocations.len())
        .sum::<usize>();
    let published = reports
        .iter()
        .map(|(_, report)| report.roots.len())
        .sum::<usize>();
    let failed = reports
        .iter()
        .filter(|(_, report)| !report.strict_succeeded || !report.diagnostics_succeeded)
        .count();
    let publishing_directories = reports
        .iter()
        .filter(|(_, report)| !report.roots.is_empty())
        .count();
    let totals = format!(
        "{} directories; {invocations} invocations; {published} publications in {publishing_directories} directories; {failed} failed directories",
        reports.len()
    );
    for (label, report) in &reports {
        check(report, &format!("{label}; {totals}"));
        assert!(
            (!report.strict_succeeded
                && !report.plain_strict_succeeded
                && !report.diagnostics_succeeded
                && !report.plain_diagnostics_succeeded)
                || !report.invocations.is_empty(),
            "{label}: neither an identical failure nor a resolver invocation; {totals}"
        );
    }
    assert!(
        failed > 0 && publishing_directories * 4 >= reports.len() * 3,
        "vacuous corpus comparison: {totals}"
    );
}

#[test]
fn enriched_kind_does_no_extra_resolution_work() {
    // Fixed inputs selected for actual event coverage, not file size. No new
    // fixture directory is needed: arithmetic and present coalescing reuse the
    // already-recorded cell sources (including erased-reference row 25l-i).
    const WORK_FIXTURES: &[&str] = &[
        "carina-cli/tests/fixtures/plan_display/plan_image_reference_layout",
        "carina-cli/tests/fixtures/plan_display/upstream_state_map_subscript",
        "carina-core/tests/fixtures/diagnostic_order/functions/duplicates",
        "carina-cli/tests/fixtures/plan_display/string_dot_suffix",
        "carina-cli/tests/fixtures/plan_display/deferred_for_with_unrelated_delete",
        "carina-cli/tests/fixtures/plan_display/resolved_value_constraint_multifile",
    ];
    let mut records = Vec::new();
    for relative in WORK_FIXTURES {
        let report = observe(&repo().join(relative));
        check(&report, relative);
        assert!(
            !report.invocations.is_empty(),
            "{relative}: no resolver coverage"
        );
        records.push(report);
    }
    let cells: Json =
        serde_json::from_str(include_str!("fixtures/authored_reference_cells.json")).unwrap();
    for id in ["10b-i", "25l-i", "5a"] {
        let row = cells["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["row"] == id)
            .unwrap();
        let cell = row["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|cell| cell["known"] == true)
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        for file in cell["files"].as_array().unwrap() {
            let path = directory.path().join(file[0].as_str().unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file[1].as_str().unwrap()).unwrap();
        }
        let label = format!("recorded cell {id} known");
        let report = observe(directory.path());
        check(&report, &label);
        assert!(
            !report.invocations.is_empty(),
            "{label}: no resolver coverage"
        );
        records.push(report);
    }
    // Every counted resolution event has an existing input above.
    // There are no asserted-zero exceptions. A new event needs a real input
    // or an explicit, justified zero exception.
    for (index, event) in EVENTS.iter().enumerate() {
        let total = records
            .iter()
            .flat_map(|report| &report.invocations)
            .map(|invocation| invocation.oracle_counts.0[index])
            .sum::<usize>();
        assert!(total > 0, "CI input set never exercised {event}");
    }
    for (index, event) in STORAGE_EVENTS.iter().enumerate() {
        let mut enriched = 0;
        for report in &records {
            for invocation in &report.invocations {
                assert_eq!(
                    invocation.oracle_storage_counts.0[index], 0,
                    "plain kind emitted {event} in {} invocation {}",
                    invocation.entry, invocation.ordinal
                );
                enriched += invocation.storage_counts.0[index];
            }
        }
        assert!(enriched > 0, "enriched kind never exercised {event}");
    }
}
