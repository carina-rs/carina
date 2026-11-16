//! Full-corpus and per-invocation work observations through production entries.
use carina_core::test_utils::resolver_kind::{
    EVENTS, InvocationKind, ProjectionReport, STORAGE_EVENTS, compare_directory,
};
use carina_core::{EnvironmentSnapshot, parser::ProviderContext, paths::ResolvedPath};
use serde_json::{Value as Json, json};
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
fn manifest() -> Json {
    serde_json::from_str(include_str!("fixtures/resolver-kind-directories.json")).unwrap()
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
    let schemas = carina_cli::fixture_plan::resolver_kind_fixture_schemas(path);
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
fn summary(label: &str, report: &ProjectionReport) -> Json {
    json!({"directory":label,"roots":report.roots.len(),"strict_error":report.strict_error,"diagnostics_error":report.diagnostics_error,
        "invocations":report.invocations.iter().map(|invocation|json!({
            "ordinal":invocation.ordinal,"entry":invocation.entry,"kind":match invocation.kind {InvocationKind::PerFile=>"per_file",InvocationKind::Merged=>"merged"},
            "errored":invocation.errored,"plain":invocation.oracle_counts.0,"enriched":invocation.counts.0,
            "plain_storage":invocation.oracle_storage_counts.0,"enriched_storage":invocation.storage_counts.0,
        })).collect::<Vec<_>>()})
}
fn save(name: &str, document: &Json) {
    if let Some(root) = std::env::var_os("CARINA_RESOLVER_KIND_REPORT_DIR") {
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            Path::new(&root).join(name),
            serde_json::to_vec_pretty(document).unwrap(),
        )
        .unwrap();
    }
}
#[test]
fn all_fixture_directories_project_identically() {
    let manifest = manifest();
    let listed = manifest["directories"].as_array().unwrap();
    assert_eq!(listed.len(), 157);
    let expected = listed
        .iter()
        .map(|item| item["directory"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(expected.len(), 157);
    let mut actual = BTreeSet::new();
    for root in [
        "carina-cli/tests/fixtures/plan_display",
        "carina-core/tests/fixtures/diagnostic_order",
    ] {
        directories(&repo().join(root), &mut actual);
    }
    assert_eq!(expected, actual, "fixture directory manifest drift");
    assert_eq!(
        listed
            .iter()
            .filter(|item| item["directory"]
                .as_str()
                .unwrap()
                .starts_with("carina-cli/"))
            .count(),
        135
    );
    assert_eq!(
        listed
            .iter()
            .filter(|item| item["nested"].as_bool().unwrap()
                && item["directory"]
                    .as_str()
                    .unwrap()
                    .starts_with("carina-cli/"))
            .count(),
        20
    );
    let mut records = Vec::new();
    for item in listed {
        let label = item["directory"].as_str().unwrap();
        let report = observe(&repo().join(label));
        check(&report, label);
        records.push(summary(label, &report));
    }
    save(
        "projection-corpus.json",
        &json!({"directories":records,"events":EVENTS,"storage_events":STORAGE_EVENTS}),
    );
    for (item, record) in listed.iter().zip(&records) {
        assert_eq!(
            item["expected_invocations"].as_u64(),
            Some(record["invocations"].as_array().unwrap().len() as u64),
            "{}: invocation coverage manifest",
            item["directory"].as_str().unwrap()
        );
        assert_eq!(
            item["expected_roots"].as_u64(),
            record["roots"].as_u64(),
            "{}: published root coverage manifest",
            item["directory"].as_str().unwrap()
        );
        assert_eq!(
            item["strict_error_expected"].as_bool(),
            Some(!record["strict_error"].is_null())
        );
        assert_eq!(
            item["diagnostics_error_expected"].as_bool(),
            Some(!record["diagnostics_error"].is_null())
        );
        for field in ["strict_error", "diagnostics_error"] {
            let actual = record[field]
                .as_str()
                .map(|error| error.replace(repo().to_str().unwrap(), "<REPO>"));
            assert_eq!(
                item[field].as_str(),
                actual.as_deref(),
                "{}: {field} manifest",
                item["directory"].as_str().unwrap()
            );
        }
    }
    eprintln!(
        "157 directories; {} invocations; {} successful roots compared",
        records
            .iter()
            .map(|r| r["invocations"].as_array().unwrap().len())
            .sum::<usize>(),
        records
            .iter()
            .map(|r| r["roots"].as_u64().unwrap())
            .sum::<u64>()
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
        records.push(summary(relative, &report));
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
        records.push(summary(&label, &report));
    }
    let ci_records = records.len();
    // Every currently counted resolution event has an existing input above.
    // There are no asserted-zero exceptions. A new event must get a real input
    // (or an explicit, justified zero exception) rather than silently passing.
    for (index, event) in EVENTS.iter().enumerate() {
        let total = records
            .iter()
            .flat_map(|record| record["invocations"].as_array().unwrap())
            .map(|invocation| invocation["plain"][index].as_u64().unwrap())
            .sum::<u64>();
        assert!(total > 0, "CI input set never exercised {event}");
    }
    let external = if let Some(checkout) = std::env::var_os("CARINA_RESOLVER_KIND_INFRA") {
        let checkout = PathBuf::from(checkout);
        let path = if checkout.join("envs/registry/dev/publish").is_dir() {
            checkout.join("envs/registry/dev/publish")
        } else {
            checkout
        };
        use carina_cli::{
            runtime::{BuiltinMockInputs, CommandInputs, CommandRuntime, InvocationHooks},
            wiring::TrustedLocalGrantInputs,
        };
        let temporary = carina_core::TestUserDirs::new();
        let mut user_dirs = (*temporary).clone();
        if let Some(home) = std::env::var_os("CARINA_RESOLVER_KIND_HOME") {
            user_dirs = user_dirs.with_trust_record_dir(Path::new(&home).join(".carina/trust"));
        }
        let runtime = CommandRuntime::new(
            user_dirs,
            CommandInputs {
                environment: EnvironmentSnapshot::empty(),
                builtin_mock: BuiltinMockInputs::deferred(|| {
                    Ok(carina_provider_mock::ConfiguredBuiltinMock::new(
                        Default::default(),
                        carina_provider_mock::MockScope::memory(),
                    ))
                }),
                trusted_local_grants: TrustedLocalGrantInputs::new(Vec::new(), None),
                accepted_egress: Default::default(),
                color_mode: carina_cli::output_style::ColorMode::Plain,
                cursor: carina_cli::cursor::CursorState::new(),
                hooks: InvocationHooks::default(),
            },
        );
        let executor = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let resolved = ResolvedPath::new(&path).unwrap();
        let (context, schemas, imports) = executor
            .block_on(async {
                carina_cli::fixture_plan::resolver_kind_installed_catalog(&runtime, &resolved)
            })
            .unwrap_or_else(|error| {
                panic!("external infra schema-only loading unavailable: {error}")
            });
        let mut paths = BTreeSet::from([path]);
        paths.extend(imports);
        let start = records.len();
        for (index, path) in paths.into_iter().enumerate() {
            // Imports are additionally loaded as roots, just as the corpus's
            // nested fixtures are. Root-only errors are retained in the report.
            let label = format!("external infra root {index}");
            let report = compare_directory(&ResolvedPath::new(&path).unwrap(), &context, &schemas);
            check(&report, &label);
            records.push(summary(&label, &report));
        }
        assert!(
            records[start..]
                .iter()
                .any(|record| record["roots"].as_u64().unwrap() > 0),
            "external infra: no resolver coverage"
        );
        json!({"status":"exercised","roots":records.len()-start,"schema_loading":"production validate schema-only; imports also loaded as roots"})
    } else {
        eprintln!(
            "SKIPPED external infra: CARINA_RESOLVER_KIND_INFRA is unset; event-covering in-repo inputs were exercised"
        );
        json!({"status":"skipped","reason":"CARINA_RESOLVER_KIND_INFRA is unset"})
    };
    let document = json!({"external":external,"events":EVENTS,"storage_events":STORAGE_EVENTS,"configurations":records,"ci_configurations":ci_records});
    let name = if std::env::var_os("CARINA_RESOLVER_KIND_INFRA").is_some() {
        "work-counts-infra.json"
    } else {
        "work-counts-ci.json"
    };
    save(name, &document);
    // Only numeric work counts leave the process; no resolved values are formatted.
    for record in &records {
        for entry in ["strict", "diagnostics"] {
            let mut plain = [0usize; EVENTS.len()];
            let mut enriched = [0usize; EVENTS.len()];
            for invocation in record["invocations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|invocation| invocation["entry"] == entry)
            {
                for i in 0..EVENTS.len() {
                    plain[i] += invocation["plain"][i].as_u64().unwrap() as usize;
                    enriched[i] += invocation["enriched"][i].as_u64().unwrap() as usize;
                }
            }
            eprintln!(
                "{} {entry}: plain {plain:?}; enriched {enriched:?}",
                record["directory"].as_str().unwrap()
            );
        }
    }
}
