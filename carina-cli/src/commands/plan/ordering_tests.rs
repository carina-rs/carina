use super::*;
use carina_core::resource::ResolvedResource;
use indexmap::IndexMap;
use serde_json::value::RawValue;

fn raw_at(json: &str, path: &[&str]) -> Box<RawValue> {
    let mut value = RawValue::from_string(json.to_owned()).unwrap();
    for segment in path {
        value = if let Ok(index) = segment.parse::<usize>() {
            serde_json::from_str::<Vec<Box<RawValue>>>(value.get())
                .unwrap()
                .remove(index)
        } else {
            serde_json::from_str::<IndexMap<String, Box<RawValue>>>(value.get())
                .unwrap()
                .swap_remove(*segment)
                .unwrap_or_else(|| panic!("missing {segment} in {}", value.get()))
        };
    }
    value
}

fn assert_keys(json: &str, path: &[&str], minimum: usize) {
    let raw = raw_at(json, path);
    let object: IndexMap<String, Box<RawValue>> = serde_json::from_str(raw.get()).unwrap();
    let keys: Vec<_> = object.keys().cloned().collect();
    assert!(keys.len() >= minimum, "{path:?}: {keys:?}");
    let mut alphabetical = keys.clone();
    alphabetical.sort();
    assert_eq!(keys, alphabetical, "{path:?}");
}

struct CopiedFixture {
    _root: tempfile::TempDir,
    path: PathBuf,
}

impl CopiedFixture {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn copy_fixture(name: &str) -> CopiedFixture {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(name);
    std::fs::create_dir(&dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plan_display")
        .join(name);
    std::fs::copy(fixture.join("main.crn"), dir.join("main.crn")).unwrap();
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("carina.state.json")).unwrap()).unwrap();
    for row in state["resources"].as_array_mut().unwrap() {
        let attrs = row.get_mut("attributes");
        if let Some(attrs) = attrs {
            for key in [
                "zulu_extra",
                "alpha_extra",
                "theta_extra",
                "beta_extra",
                "gamma_extra",
                "delta_extra",
                "epsilon_extra",
                "eta_extra",
            ] {
                attrs[key] = serde_json::json!(key);
            }
        } else {
            for key in [
                "zulu_extra",
                "alpha_extra",
                "theta_extra",
                "beta_extra",
                "gamma_extra",
                "delta_extra",
                "epsilon_extra",
                "eta_extra",
            ] {
                row["current"]["attributes"][key] = serde_json::json!(key);
            }
        }
    }
    std::fs::write(
        dir.join("carina.state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    CopiedFixture {
        _root: root,
        path: dir,
    }
}

fn fixture_document(path: &Path) -> String {
    try_fixture_document(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .json
}

struct FixtureDocument {
    json: String,
    saved: Vec<u8>,
}

fn try_fixture_document(path: &Path) -> Result<FixtureDocument, String> {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let fixture = crate::fixture_plan::build_plan_from_fixture_path(&runtime, path)
        .map_err(|error| error.to_string())?;
    if fixture.checked.is_none() {
        return Err(fixture
            .plan
            .errors()
            .iter()
            .map(|error| error.display_with_project_dir(path).to_string())
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let schemas = fixture
        .schemas
        .iter()
        .map(|(provider, _, _, schema)| (provider.to_owned(), schema.clone()))
        .collect::<Vec<_>>();
    let validated = ValidatedConfig::for_fixture_with_additional_schemas(
        &runtime,
        (),
        Vec::new(),
        &[] as &[ProviderConfig],
        schemas,
    );
    let parsed = carina_core::parser::ExpandedParsedFile::default();
    let state = fixture.state_file;
    let ctx = crate::wiring::PlanContext {
        plan: crate::wiring::PlanOutcome::Planned(crate::wiring::PlannedPlan::Unrendered {
            plan: Box::new(fixture.checked.expect("valid fixture")).into(),
            references: carina_core::references::prepare_empty_for_tests(),
        }),
        provider: ProviderRouter::new(),
        sorted_resources: fixture.desired_resources,
        unresolved_resources: fixture.unresolved_resources,
        legacy_dependency_classification: None,
        migrated_legacy_state: None,
        compositions: Vec::new(),
        data_sources: Vec::new(),
        managed_current: fixture.managed_current,
        data_source_values: carina_core::data_source_value::DataSourceValues::empty(),
        observation_authority: crate::test_support::observation_authority_for_test(state.as_ref()),
        moved_origins: fixture.moved_origins,
        upstream_snapshot: carina_core::upstream_exports::remote_bindings_for_test(
            ["zeta", "alpha", "theta", "beta"]
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        ["zulu", "alpha", "theta", "beta"]
                            .into_iter()
                            .map(|key| {
                                (
                                    key.to_owned(),
                                    Value::Concrete(ConcreteValue::String(key.into())),
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
        ),
        prev_explicit: fixture.prev_explicit,
        residual_deferred_for: parsed.deferred_for_expressions().empty_like(),
        expansion_trace: fixture.expansion_trace,
    };
    let provider_configs = carina_core::validation::deferred_types::with_session_for_test(
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        |session| {
            carina_core::parser::resolve_provider_attributes_with_remote(
                &parsed,
                &Default::default(),
                session,
            )
        },
    )
    .unwrap();
    let checked = build_plan_file_from_parts(
        &ResolvedPath::new(path).unwrap(),
        &parsed,
        &provider_configs,
        SavedBackendConfig::default(),
        &state,
        &ctx,
        &validated,
    )
    .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    serde_path_to_error::serialize(&checked, &mut serde_json::Serializer::new(&mut bytes))
        .map_err(|error| error.to_string())?;
    let saved = carina_core::utils::pretty_with_newline_bytes(&checked)
        .map_err(|error| error.to_string())?;
    Ok(FixtureDocument {
        json: String::from_utf8(bytes).unwrap(),
        saved,
    })
}

#[test]
fn issue_4491_plan_json_observed_and_historical_attribute_keys_are_alphabetical() {
    let dir = copy_fixture("replace_with_non_forcing_diffs");
    let json = fixture_document(dir.path());
    assert_keys(
        &json,
        &["current_states", "0", "current", "state", "attributes"],
        8,
    );
    assert_keys(
        &json,
        &[
            "plan",
            "replacement_inputs",
            "old_instances",
            "0",
            "attributes",
        ],
        8,
    );
    assert_keys(
        &json,
        &["plan", "replace_display", "0", "previous_attributes"],
        8,
    );

    let dir = copy_fixture("map_attribute_removed");
    let json = fixture_document(dir.path());
    let effects: Vec<Box<RawValue>> =
        serde_json::from_str(raw_at(&json, &["plan", "effects"]).get()).unwrap();
    let update = effects
        .iter()
        .find(|effect| effect.get().starts_with("{\"Update\""))
        .expect("update");
    assert_keys(update.get(), &["Update", "from", "attributes"], 8);
}

#[test]
fn issue_4491_saved_plan_replays_non_alphabetical_observed_keys_byte_for_byte() {
    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum HistoricalJson {
        Object(IndexMap<String, HistoricalJson>),
        Array(Vec<HistoricalJson>),
        Scalar(serde_json::Value),
    }
    fn reverse_observed_keys(json: &mut HistoricalJson) {
        match json {
            HistoricalJson::Object(fields) => {
                for (key, value) in fields {
                    reverse_observed_keys(value);
                    if matches!(key.as_str(), "attributes" | "previous_attributes")
                        && let HistoricalJson::Object(attributes) = value
                    {
                        attributes.reverse();
                    }
                }
            }
            HistoricalJson::Array(values) => {
                for value in values {
                    reverse_observed_keys(value);
                }
            }
            HistoricalJson::Scalar(_) => {}
        }
    }
    for fixture in ["replace_with_non_forcing_diffs", "map_attribute_removed"] {
        let dir = copy_fixture(fixture);
        let mut historical: HistoricalJson =
            serde_json::from_str(&fixture_document(dir.path())).unwrap();
        reverse_observed_keys(&mut historical);
        let compact = serde_json::to_string(&historical).unwrap();
        let attributes = raw_at(
            &compact,
            &["current_states", "0", "current", "state", "attributes"],
        );
        let keys: IndexMap<String, Box<RawValue>> = serde_json::from_str(attributes.get()).unwrap();
        assert!(keys.len() >= 8);
        assert!(
            !keys.keys().is_sorted(),
            "fixture must retain historical hash order"
        );
        let decoded = PlanFile::decode_json(compact.as_bytes()).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), compact);
        let recorded = carina_core::utils::pretty_with_newline_bytes(&historical).unwrap();
        let decoded = PlanFile::decode_json(&recorded).unwrap();
        assert_eq!(
            carina_core::utils::pretty_with_newline_bytes(&decoded).unwrap(),
            recorded
        );
        // A second load repeats the replay instead of replacing the recorded
        // field order with the ordered maps' iteration order.
        let replayed = serde_json::to_vec(&decoded).unwrap();
        assert_eq!(replayed, compact.as_bytes());
        assert_eq!(
            serde_json::to_vec(&PlanFile::decode_json(&replayed).unwrap()).unwrap(),
            replayed
        );
    }
}

#[test]
fn issue_4491_upstream_snapshot_both_levels_are_alphabetical() {
    let dir = copy_fixture("replace_with_non_forcing_diffs");
    let json = fixture_document(dir.path());
    assert_keys(&json, &["upstream_snapshot"], 4);
    for binding in ["alpha", "beta", "theta", "zeta"] {
        assert_keys(&json, &["upstream_snapshot", binding], 4);
    }
}

#[test]
fn issue_4491_deferred_replace_previous_attributes_are_alphabetical() {
    let dir = copy_fixture("deferred_for_with_paired_destroy");
    let fixture = crate::fixture_plan::build_plan_from_fixture_path(
        &crate::test_support::TestCommandRuntime::new(),
        dir.path(),
    )
    .unwrap();
    let json = serde_json::to_string(&carina_core::plan::SavedPlan::from(
        fixture.plan.to_snapshot(),
    ))
    .unwrap();
    let effects: Vec<Box<RawValue>> =
        serde_json::from_str(raw_at(&json, &["effects"]).get()).unwrap();
    let deferred = effects
        .iter()
        .find(|effect| effect.get().starts_with("{\"DeferredReplace\""))
        .expect("deferred replacement");
    assert_keys(
        deferred.get(),
        &["DeferredReplace", "deletes", "0", "previous_attributes"],
        8,
    );
}

#[test]
fn issue_4491_deferred_deletes_and_required_children_inherit_orphan_order() {
    let dir = copy_fixture("deferred_for_with_paired_destroy");
    let path = dir.path().join("carina.state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let original = state["resources"][0].clone();
    state["resources"] = (0..5).map(|index| {
        let mut row = original.clone();
        row["identity"] = format!("validation_records[{index}]").into();
        row["binding"] = format!("validation_records[{index}]").into();
        row["current"]["identifier"] = format!("zone:record-{index}:CNAME").into();
        if let Some(parent) = match index { 4 => Some(2), 2 => Some(0), _ => None } {
            row["dependency_bindings"].as_array_mut().unwrap().push(serde_json::json!({
                "provider": "aws", "resource_type": "route53.RecordSet", "identity": format!("validation_records[{parent}]"),
            }));
        }
        row
    }).collect();
    std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    let fixture = crate::fixture_plan::build_plan_from_fixture_path(
        &crate::test_support::TestCommandRuntime::new(),
        dir.path(),
    )
    .unwrap();
    let mut plan = fixture.plan;
    let ids = plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::DeferredReplace(payload) => Some(
                payload
                    .deletes
                    .iter()
                    .map(|old| old.id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .expect("deferred replacement");
    for (index, id) in ids.into_iter().enumerate() {
        let dependent = Resource::with_provider(
            "mock",
            "test.resource",
            carina_core::resource::ResourceIdentity::try_from(format!("dependent_{index}"))
                .unwrap(),
            None,
        )
        .into_resolved_dependency_phase_for_test();
        plan.add(Effect::Create(ResolvedResource::from_resolved_parts(
            dependent,
            [id].into_iter().collect(),
            Default::default(),
        )));
    }
    let mut schemas = fixture.schemas;
    schemas.insert(
        "mock",
        carina_core::schema::ResourceSchema::new("test.resource")
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "aws",
        carina_core::schema::ResourceSchema::new("route53.RecordSet")
            .with_coexisting_replacement()
            .validate()
            .unwrap(),
    );
    let admitted = plan.admit(&schemas).unwrap();
    let json =
        serde_json::to_value(carina_core::plan::SavedPlan::from(admitted.to_snapshot())).unwrap();
    let deferred = json["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|effect| effect.get("DeferredReplace"))
        .unwrap();
    let expected = [
        "validation_records[1]",
        "validation_records[3]",
        "validation_records[4]",
        "validation_records[2]",
        "validation_records[0]",
    ];
    assert_eq!(
        deferred["deletes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|old| old["id"]["identity"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        deferred["required_children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry[0]["identity"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn issue_4491_prefix_keys_remain_alphabetical_through_resource_wire() {
    let mut resource = Resource::with_provider("mock", "test.resource", "example", None);
    for key in ["zeta", "alpha", "theta", "beta"] {
        resource.prefixes.insert(key.into(), format!("{key}-"));
    }
    let resource = ResolvedResource::new(resource.into_value_snapshot());
    let saved = ResolvedResource::new(resource.as_inner().to_snapshot());
    let json = serde_json::to_string(&saved).unwrap();
    assert_keys(&json, &["prefixes"], 4);
    let decoded: carina_core::resource::SavedResolvedResource =
        serde_json::from_str(&json).unwrap();
    assert_keys(&serde_json::to_string(&decoded).unwrap(), &["prefixes"], 4);
    let dir = copy_fixture("replace_with_non_forcing_diffs");
    let mut wire: PlanFileWire = serde_json::from_str(&fixture_document(dir.path())).unwrap();
    wire.sorted_resources[0].prefixes = resource.prefixes.clone();
    assert_keys(
        &serde_json::to_string(&wire).unwrap(),
        &["sorted_resources", "0", "prefixes"],
        4,
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resource));
    let json =
        serde_json::to_string(&carina_core::plan::SavedPlan::from(plan.to_snapshot())).unwrap();
    assert_keys(&json, &["effects", "0", "Create", "prefixes"], 4);
}

#[test]
fn issue_4491_fresh_hash_states_produce_identical_plan_document_bytes_except_timestamp() {
    let dir = copy_fixture("destroy_orphans");
    let without_timestamp = |json: String| {
        let mut root: IndexMap<String, Box<RawValue>> = serde_json::from_str(&json).unwrap();
        root.swap_remove("timestamp");
        serde_json::to_vec(&root).unwrap()
    };
    let first = without_timestamp(fixture_document(dir.path()));
    for _ in 0..8 {
        assert_eq!(without_timestamp(fixture_document(dir.path())), first);
    }
}

#[test]
fn issue_4491_all_fixture_documents_are_identical_across_processes() {
    const TEST: &str = "commands::plan::ordering_tests::issue_4491_all_fixture_documents_are_identical_across_processes";
    const RUNS: usize = 8;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plan_display");
    fn collect_roots(path: &Path, roots: &mut Vec<PathBuf>) {
        let entries: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        if entries
            .iter()
            .any(|entry| entry.extension().is_some_and(|ext| ext == "crn"))
        {
            roots.push(path.to_owned());
        }
        for entry in entries {
            if entry.is_dir() {
                collect_roots(&entry, roots);
            }
        }
    }
    let mut roots = Vec::new();
    collect_roots(&fixtures, &mut roots);
    roots.sort();
    assert!(!roots.is_empty());
    fn assert_bytes_equal(actual: &[u8], expected: &[u8], kind: &str, root: &Path, run: usize) {
        let offset = actual
            .iter()
            .zip(expected)
            .position(|(left, right)| left != right)
            .unwrap_or(actual.len().min(expected.len()));
        assert!(
            actual == expected,
            "whole {kind} for {} differs in process {run} at byte {offset}: {:?} vs {:?}",
            root.display(),
            String::from_utf8_lossy(
                &actual[offset.saturating_sub(40)..(offset + 80).min(actual.len())]
            ),
            String::from_utf8_lossy(
                &expected[offset.saturating_sub(40)..(offset + 80).min(expected.len())]
            )
        );
    }
    if let Some(output_dir) = std::env::var_os("CARINA_4491_PLAN_DOCUMENTS") {
        let mut documents = Vec::new();
        let mut diagnostics = Vec::new();
        for root in &roots {
            let relative = root.strip_prefix(&fixtures).unwrap();
            eprintln!("capturing {}", relative.display());
            let loaded = carina_core::config_loader::load_configuration(
                &ResolvedPath::new(root).unwrap(),
                &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            );
            let captured = loaded.map_err(|error| error.to_string()).and_then(|_| {
                carina_core::test_utils::with_secret_hash_salt_for_test([42; 16], || {
                    try_fixture_document(root)
                })
            });
            let path = Path::new(&output_dir).join(relative).with_extension("json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let json = match captured {
                Ok(captured) => {
                    documents.push(relative.to_owned());
                    std::fs::write(
                        path.with_extension("saved.json"),
                        without_timestamp(&captured.saved),
                    )
                    .unwrap();
                    captured.json
                }
                Err(error) => {
                    // The corpus includes standalone modules, diagnostic plans,
                    // and unsaveable pending identities (#4491 records these).
                    // Compare their complete outcomes too; never skip a root.
                    diagnostics.push(relative.to_owned());
                    serde_json::to_string(&serde_json::json!({"error": error})).unwrap()
                }
            };
            std::fs::write(path, without_timestamp(json.as_bytes())).unwrap();
        }
        std::fs::write(
            Path::new(&output_dir).join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "documents": documents, "diagnostics": diagnostics,
            }))
            .unwrap(),
        )
        .unwrap();
        return;
    }
    let output = tempfile::tempdir().unwrap();
    let mut first = Vec::new();
    let mut first_saved = Vec::new();
    let mut first_manifest = Vec::new();
    for run in 0..RUNS {
        let capture_dir = output.path().join(run.to_string());
        std::fs::create_dir(&capture_dir).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("CARINA_4491_PLAN_DOCUMENTS", &capture_dir)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "process {run}: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let manifest = std::fs::read(capture_dir.join("manifest.json")).unwrap();
        let coverage: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(
            coverage["documents"].as_array().unwrap().len()
                + coverage["diagnostics"].as_array().unwrap().len(),
            roots.len()
        );
        if run == 0 {
            first_manifest = manifest;
        } else {
            assert_eq!(
                manifest, first_manifest,
                "all root outcomes must be covered in every process"
            );
        }
        for (index, root) in roots.iter().enumerate() {
            let path = capture_dir
                .join(root.strip_prefix(&fixtures).unwrap())
                .with_extension("json");
            let document = std::fs::read(&path).unwrap();
            let saved_path = path.with_extension("saved.json");
            let saved = saved_path
                .exists()
                .then(|| std::fs::read(saved_path).unwrap());
            if run == 0 {
                first.push(document);
                first_saved.push(saved);
            } else {
                match (saved.as_deref(), first_saved[index].as_deref()) {
                    (Some(actual), Some(expected)) => {
                        assert_bytes_equal(actual, expected, "saved plan", root, run)
                    }
                    (None, None) => {}
                    _ => panic!(
                        "saved-plan availability for {} differs in process {run}",
                        root.display()
                    ),
                }
                assert_bytes_equal(&document, &first[index], "JSON document", root, run);
            }
        }
    }
    let coverage: serde_json::Value = serde_json::from_slice(&first_manifest).unwrap();
    eprintln!(
        "compared {} roots in {RUNS} fresh processes: {} documents and saved plans, {} diagnostic outcomes",
        roots.len(),
        coverage["documents"].as_array().unwrap().len(),
        coverage["diagnostics"].as_array().unwrap().len()
    );
}

fn without_timestamp(json: &[u8]) -> Vec<u8> {
    // Borrow the raw value from the original input, so every other byte,
    // including envelope whitespace and the saved writer's newline, survives.
    let fields: IndexMap<&str, &RawValue> = serde_json::from_slice(json).unwrap();
    let Some(timestamp) = fields.get("timestamp") else {
        return json.to_vec();
    };
    let raw = timestamp.get();
    let start = raw.as_ptr().addr() - json.as_ptr().addr();
    let mut normalized = json[..start].to_vec();
    normalized.extend_from_slice(b"null");
    normalized.extend_from_slice(&json[start + raw.len()..]);
    normalized
}

#[test]
fn issue_4491_plan_document_comparison_preserves_other_bytes() {
    let document = b"{\n  \"z\" : \"same\", \"timestamp\" : \"same\", \"nested\" : { \"timestamp\": \"same\" }, \"a\": 3\n}\n";
    let expected = b"{\n  \"z\" : \"same\", \"timestamp\" : null, \"nested\" : { \"timestamp\": \"same\" }, \"a\": 3\n}\n";
    assert_eq!(without_timestamp(document), expected);
    let diagnostic = b"{ \"error\" : \"expected failure\" }\n";
    assert_eq!(without_timestamp(diagnostic), diagnostic);
}
