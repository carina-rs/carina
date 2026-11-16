//! Pinned main saved-plan documents through fixture planning and the CLI builder.
//! Nested fixture directories are also attempted as roots; early root-load
//! failures and diagnostic-only plans are explicit manifest entries.
use super::*;
use carina_core::provider::{BoxFuture, ProviderFactory, ProviderInitialization, ProviderResult};
use carina_core::schema::{
    AttributeSchema, AttributeType, DslTransform, ResourceSchema, TypeIdentity,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use serde_json::{Value as Json, json};
use std::cell::RefCell;

#[path = "../../../tests/support/resolver_kind_main.rs"]
mod baseline;

// The fixture's factory callback supplies the actual schema witnesses. Retain
// copies for the separate validation/evidence adapter, without reaching into
// wiring's private fields or loading a plugin.
#[derive(Clone)]
struct SchemaFactory {
    name: String,
    witness: ValidatedProviderSchemaWitness,
}
static VALIDATION_FACTORIES: std::sync::OnceLock<Vec<SchemaFactory>> = std::sync::OnceLock::new();

// Exact schema data from main fixture_plan's private validation catalog.
// Preflight uses its fallible production transition because the fixture helper
// unwraps errors. Saved-plan evidence still uses the real fixture's schemas.
fn fixture_validation_enum_schemas(fixture_path: &Path) -> Vec<(String, ValidatedResourceSchema)> {
    let mut schemas = match fixture_path.file_name().and_then(|name| name.to_str()) {
        Some("dynamic_enum_az_no_diff") => vec![(
            "fixture".to_string(),
            ResourceSchema::new("network.Subnet").attribute(
                AttributeSchema::required("availability_zone", fixture_zone_name_type()).build(),
            ),
        )],
        Some("enum_display" | "no_changes_enum") => vec![(
            "awscc".to_string(),
            ResourceSchema::new("ec2.Vpc")
                .attribute(
                    AttributeSchema::required("cidr_block", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .attribute(
                    AttributeSchema::required("instance_tenancy", fixture_instance_tenancy_type())
                        .build(),
                ),
        )],
        _ => Vec::new(),
    };
    schemas.push((
        "aws".to_string(),
        ResourceSchema::new("route53.RecordSet").attribute(
            AttributeSchema::optional("type", fixture_route53_record_type())
                .create_only()
                .build(),
        ),
    ));
    schemas
        .into_iter()
        .map(|(provider, schema)| {
            (
                provider,
                schema
                    .validate()
                    .expect("fixture validation schema must be valid"),
            )
        })
        .collect()
}

fn fixture_zone_name_type() -> AttributeType {
    AttributeType::enum_(
        TypeIdentity::new(
            Some("fixture"),
            vec!["network".to_string(), "Subnet".to_string()],
            "ZoneName",
        ),
        None,
        vec![],
        None,
        Some(DslTransform::HyphenToUnderscore),
    )
}

fn fixture_instance_tenancy_type() -> AttributeType {
    AttributeType::enum_(
        TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
        Some(vec![
            "default".to_string(),
            "dedicated".to_string(),
            "host".to_string(),
        ]),
        vec![],
        None,
        None,
    )
}

fn fixture_route53_record_type() -> AttributeType {
    AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["route53"], "RecordType"),
        Some(vec!["CNAME".to_string()]),
        vec![("CNAME".to_string(), "cname".to_string())],
        None,
        None,
    )
}
impl ProviderFactory for SchemaFactory {
    fn name(&self) -> &str {
        &self.name
    }
    fn display_name(&self) -> &str {
        "PR 1 fixture schema witness"
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        self.witness.resources().to_vec()
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        self.witness.clone()
    }
    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { panic!("schema-only adapter must not initialize a provider") })
    }
}
fn loaded(factories: &[SchemaFactory]) -> Vec<crate::wiring::LoadedProviderFactory> {
    crate::wiring::LoadedProviderFactory::for_test(
        factories
            .iter()
            .cloned()
            .map(|factory| Box::new(factory) as Box<dyn ProviderFactory>)
            .collect(),
    )
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plan_display")
}
fn fixture_directories() -> Vec<String> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<String>) {
        let mut has_crn = false;
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), output);
            } else if entry.path().extension().is_some_and(|ext| ext == "crn") {
                has_crn = true;
            }
        }
        if has_crn {
            output.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
        }
    }
    let root = fixture_root();
    let mut dirs = Vec::new();
    visit(&root, &root, &mut dirs);
    dirs.sort();
    assert_eq!(dirs.len(), 135);
    dirs
}
fn error(stage: &str, error: impl std::fmt::Display) -> Json {
    baseline::normalize_prefix(
        json!({"stage": stage, "rendered": error.to_string()}),
        &baseline::workspace(),
        "<WORKSPACE>",
    )
}

fn document(name: &str) -> Result<String, Json> {
    let path = fixture_root().join(name);
    let project = ResolvedPath::new(&path).unwrap();
    let runtime = crate::test_support::TestCommandRuntime::new();
    let config = ProviderContext::new(runtime.environment().clone());
    // The fixture helper unwraps early loader errors. Observe those via the
    // same production loader before calling it; a panic is never an error row.
    let initial = load_configuration_with_config(&project, &config, &Default::default())
        .map_err(|e| error("root-load", e))?;
    let canonical = VALIDATION_FACTORIES
        .get()
        .map(|factories| {
            ProviderLoad::for_fixture_with_additional_schemas(
                &runtime,
                loaded(factories),
                &initial.parsed.providers,
                fixture_validation_enum_schemas(&path),
            )
            .validate_config_pair(
                initial.parsed.clone(),
                initial.unresolved_parsed.clone(),
                &project,
                true,
                &initial.duplicate_declarations,
            )
            .map(|validated| validated.into_resources_for_fixture())
            .map_err(|e| {
                error(
                    "root-validation",
                    crate::error::format_error_with_source_chain(&e),
                )
            })
        })
        .transpose()?;
    let catalogs = RefCell::new(Vec::<Vec<SchemaFactory>>::new());
    let fixture =
        crate::fixture_plan::build_plan_from_fixture_path_using(&runtime, &path, |factories| {
            let witnesses = factories
                .iter()
                .map(|factory| SchemaFactory {
                    name: factory.name().to_owned(),
                    witness: factory.schema_witness(),
                })
                .collect::<Vec<_>>();
            if catalogs.borrow().is_empty() {
                VALIDATION_FACTORIES.get_or_init(|| witnesses.clone());
            }
            catalogs.borrow_mut().push(witnesses);
            crate::wiring::LoadedProviderFactory::for_test(factories)
        })
        .map_err(|e| {
            error(
                "fixture-plan",
                crate::error::format_error_with_source_chain(&e),
            )
        })?;
    let checked = fixture.checked.clone().ok_or_else(|| {
        baseline::normalize_prefix(
            json!({"stage": "plan-diagnostics", "errors": fixture.plan.errors().iter().map(|error| error.display_with_project_dir(&path).to_string()).collect::<Vec<_>>()}),
            &baseline::workspace(), "<WORKSPACE>")
    })?;
    let catalogs = catalogs.into_inner();
    assert_eq!(
        catalogs.len(),
        2,
        "fixture validation and planning catalogs"
    );
    let schemas = fixture
        .schemas
        .iter()
        .map(|(provider, _, _, schema)| (provider.to_owned(), schema.clone()))
        .collect::<Vec<_>>();
    // #4478: obtain the owned canonical pair through validation; do not peel
    // PrefixCanonicalized or manufacture CanonicalResource values.
    let canonical = canonical.unwrap_or_else(|| {
        // Only the first (all_create) fixture bootstraps the factory witnesses.
        // Its helper has just succeeded; an adapter error here must fail.
        ProviderLoad::for_fixture_with_additional_schemas(
            &runtime,
            loaded(&catalogs[0]),
            &initial.parsed.providers,
            fixture_validation_enum_schemas(&path),
        )
        .validate_config_pair(
            initial.parsed,
            initial.unresolved_parsed,
            &project,
            true,
            &initial.duplicate_declarations,
        )
        .expect("fixture save context must reproduce successful validation")
        .into_resources_for_fixture()
    });
    let parsed = canonical.parsed();
    let validated = ValidatedConfig::for_fixture_with_additional_schemas(
        &runtime,
        (),
        // fixture_plan loads aws/awscc schema-only factories even when no
        // plugin source is selected. They are schema evidence, not artifacts
        // used by the saved configuration. Retain their schemas above, and
        // load only source-selected factories for artifact capture.
        loaded(
            &catalogs[1]
                .iter()
                .filter(|factory| {
                    parsed.providers.iter().any(|config| {
                        config.name == factory.name
                            && provider_factory_source(&config.loading_projection()).is_some()
                    })
                })
                .cloned()
                .collect::<Vec<_>>(),
        ),
        &parsed.providers,
        schemas,
    );
    // Match fixture_plan's local, published-state construction, including an
    // admitted empty binding when the upstream has no state. No backend I/O.
    let mut remote = AdmittedRemoteBindings::new();
    for upstream in &parsed.upstream_states {
        let state_path = project
            .join(&upstream.source)
            .unwrap()
            .join(carina_state::LocalBackend::DEFAULT_STATE_FILE)
            .unwrap();
        let exports = state_path
            .read()
            .ok()
            .map(|bytes| carina_state::PublishedExports::from_state_bytes(&bytes))
            .transpose()
            .map_err(|e| error("upstream-state", e))?;
        let bindings =
            carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
                let source = carina_core::upstream_exports::ResolvedUpstreamSource::for_test(
                    upstream.binding.as_str(),
                    None,
                );
                match exports {
                    Some(exports) => source.decode_and_admit(
                        exports.values().iter().map(|(k, v)| (k.clone(), v)),
                        session,
                    ),
                    None => source.admit_snapshot(HashMap::new(), session),
                }
            })
            .map_err(|e| error("upstream-admission", e))?;
        remote.insert(upstream.binding.to_string(), bindings);
    }
    let provider_configs =
        carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
            carina_core::parser::resolve_provider_attributes_with_remote(parsed, &remote, session)
        })
        .map_err(|e| error("provider-config", e))?;
    let data_sources = parsed.data_sources.clone();
    let data_source_values = carina_core::data_source_value::DataSourceValues::from_data_sources(
        &data_sources,
        |source| {
            DataSourceValue::read(
                fixture
                    .data_source_states
                    .get(&source.id)
                    .expect("fixture read-state coverage")
                    .clone(),
            )
        },
    );
    let backend = SavedBackendConfig::from(parsed.backend.clone());
    let ctx = crate::wiring::PlanContext {
        plan: crate::wiring::PlanOutcome::Planned(crate::wiring::PlannedPlan::Unrendered {
            plan: Box::new(checked),
            references: carina_core::references::prepare_empty_for_tests(),
        }),
        provider: ProviderRouter::new(),
        sorted_resources: fixture.desired_resources,
        unresolved_resources: fixture.unresolved_resources,
        compositions: fixture.compositions,
        data_sources,
        data_source_values,
        managed_current: fixture.managed_current,
        moved_origins: fixture.moved_origins,
        upstream_snapshot: remote,
        prev_explicit: fixture.prev_explicit,
        residual_deferred_for: fixture.deferred_for_expressions,
        expansion_trace: fixture.expansion_trace,
        legacy_dependency_classification: None,
        migrated_legacy_state: None,
    };
    let mut saved = build_plan_file_from_parts(
        &project,
        parsed,
        &provider_configs,
        backend,
        &fixture.state_file,
        &ctx,
        &validated,
    )
    .map_err(|e| match e {
        PlanSaveError::ProviderEvidence(error) => {
            panic!("fixture evidence adapter failed: {error}")
        }
        error_value => error("plan-save", error_value),
    })?;
    saved.contents.timestamp = "2000-01-01T00:00:00+00:00".to_owned();
    // Exactly the plan command's serializer. Preserve all map/array order.
    let bytes =
        serde_json::to_string_pretty(&saved).map_err(|e| error("plan-serialize", e))? + "\n";
    Ok(bytes.replace(baseline::workspace().to_str().unwrap(), "<WORKSPACE>"))
}

// PHC comparison never parses and reserializes the document. Locate and
// verify hashes through a read-only JSON view, then replace those exact string
// tokens in the original bytes. The per-path material and PHC shape remain
// pinned in the manifest. Legacy fixture markers are compared byte for byte.
fn verified_phcs(bytes: &str, name: &str) -> (String, Json) {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    fn visit(value: &Json, pointer: String, hashes: &mut Vec<(String, String)>) {
        match value {
            Json::String(text) if text.starts_with("$argon2") => {
                hashes.push((pointer, text.clone()))
            }
            Json::Array(values) => {
                for (i, v) in values.iter().enumerate() {
                    visit(v, format!("{pointer}/{i}"), hashes);
                }
            }
            Json::Object(values) => {
                for (k, v) in values {
                    visit(
                        v,
                        format!("{pointer}/{}", k.replace('~', "~0").replace('/', "~1")),
                        hashes,
                    );
                }
            }
            _ => {}
        }
    }
    let materials: &[&str] = match name {
        "secret_values" => &["my-secret-password"],
        // old-secret is the historical material in this fixture's state;
        // the snapshot test also names it in its no-plaintext assertion.
        "nested_write_only" => &[
            "accepted-secret",
            "old-secret",
            "new-secret",
            "new-replace-secret",
        ],
        _ => &[],
    };
    let parsed: Json = serde_json::from_str(bytes).unwrap();
    let mut hashes = Vec::new();
    visit(&parsed, String::new(), &mut hashes);
    if !materials.is_empty() {
        assert!(!hashes.is_empty(), "{name}: missing secret hashes");
    }
    let mut normalized = bytes.to_owned();
    let mut records = serde_json::Map::new();
    for (pointer, encoded) in hashes {
        let phc = PasswordHash::new(&encoded).expect("valid fixture PHC");
        let material = materials
            .iter()
            .find(|material| {
                let material = serde_json::to_vec(material).unwrap();
                Argon2::default().verify_password(&material, &phc).is_ok()
            })
            .unwrap_or_else(|| {
                panic!("{name}{pointer}: PHC does not verify against fixture material")
            });
        records.insert(
            pointer,
            json!({"material": material, "algorithm": phc.algorithm.as_str(),
            "version": phc.version, "params": phc.params.to_string(),
            "salt_chars": phc.salt.unwrap().len(), "digest_bytes": phc.hash.unwrap().len()}),
        );
        normalized = normalized.replace(
            &serde_json::to_string(&encoded).unwrap(),
            "\"<VERIFIED-PHC>\"",
        );
    }
    (normalized, Json::Object(records))
}

#[test]
#[ignore = "main-only exporter; never called by the consumer"]
fn record_main_plan_json() {
    let header = baseline::recording_header();
    let root = baseline::golden_root().join("plan-json");
    std::fs::create_dir_all(&root).unwrap();
    let mut entries = Vec::new();
    for name in fixture_directories() {
        eprintln!("record plan fixture: {name}");
        match document(&name) {
            Ok(bytes) => {
                let (_, phcs) = verified_phcs(&bytes, &name);
                let file = format!("{}.json", name.replace('/', "__"));
                std::fs::write(root.join(&file), bytes).unwrap();
                entries.push(json!({"fixture": name, "file": file, "verified_phcs": phcs}));
            }
            Err(expected) => {
                eprintln!("expected error in {name}: {expected}");
                entries.push(json!({"fixture": name, "expected_error": expected}));
            }
        }
    }
    let documents = entries.iter().filter(|e| e.get("file").is_some()).count();
    assert!(documents > 0, "empty saved-plan coverage is not a baseline");
    let mut secret_documents = 0;
    for name in ["secret_values", "nested_write_only"] {
        let entry = entries
            .iter()
            .find(|entry| entry["fixture"] == name)
            .unwrap();
        if entry.get("file").is_some() {
            secret_documents += 1;
            assert!(
                !entry["verified_phcs"].as_object().unwrap().is_empty(),
                "{name}: PHC coverage missing"
            );
        }
    }
    assert!(secret_documents > 0, "missing PHC document coverage");
    let manifest = json!({"header": header, "directories": 135, "documents": documents,
        "expected_errors": entries.len()-documents,
        "normalization": {"timestamp": "2000-01-01T00:00:00+00:00", "root": "<WORKSPACE>",
            "secrets": "verify candidate and golden PHCs at every path against fixture material; pin PHC metadata, replace full PHC strings with <VERIFIED-PHC> for comparison"},
        "nested_directories": "attempted as roots, not as module-loader coverage",
        "entries": entries});
    std::fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    eprintln!(
        "{documents} plan documents, {} expected errors",
        135 - documents
    );
}

#[test]
fn plan_json_matches_main() {
    // These array orders vary between runs of main dedb7bceb itself, measured
    // in Phase 0. This list must not grow to make head pass.
    const UNORDERED_ARRAY_POINTERS: &[&str] =
        &["/plan/effects", "/plan/replacement_inputs/old_instances"];

    fn comparison_value(value: &Json, pointer: &str) -> Json {
        match value {
            Json::Object(values) => {
                // Sort explicitly so equality and serialization do not depend
                // on serde_json's preserve_order feature being enabled or off.
                let mut fields = values.iter().collect::<Vec<_>>();
                fields.sort_by_key(|(left, _)| *left);
                Json::Object(
                    fields
                        .into_iter()
                        .map(|(key, value)| {
                            let escaped = key.replace('~', "~0").replace('/', "~1");
                            (
                                key.clone(),
                                comparison_value(value, &format!("{pointer}/{escaped}")),
                            )
                        })
                        .collect(),
                )
            }
            Json::Array(values) => {
                let values = values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| comparison_value(value, &format!("{pointer}/{index}")))
                    .collect::<Vec<_>>();
                if UNORDERED_ARRAY_POINTERS.contains(&pointer) {
                    let mut elements = values
                        .into_iter()
                        .map(|value| (serde_json::to_string(&value).unwrap(), value))
                        .collect::<Vec<_>>();
                    elements.sort_by(|(left, _), (right, _)| left.cmp(right));
                    // Keep duplicates: these locations are multisets, not sets.
                    Json::Array(elements.into_iter().map(|(_, value)| value).collect())
                } else {
                    Json::Array(values)
                }
            }
            value => value.clone(),
        }
    }

    // Pin the comparison boundary: permitted permutations keep multiplicity,
    // while nested and all other arrays retain their original order.
    let sample = json!({"plan": {"effects": [{"values": [1, 2]}, {"values": [3]}],
        "replacement_inputs": {"old_instances": [1, 2, 2]}, "other": [1, 2]}});
    let mut permutation = sample.clone();
    permutation["plan"]["effects"]
        .as_array_mut()
        .unwrap()
        .reverse();
    permutation["plan"]["replacement_inputs"]["old_instances"] = json!([2, 1, 2]);
    assert_eq!(
        comparison_value(&sample, ""),
        comparison_value(&permutation, "")
    );
    let mut missing_duplicate = sample.clone();
    missing_duplicate["plan"]["replacement_inputs"]["old_instances"] = json!([1, 2]);
    assert_ne!(
        comparison_value(&sample, ""),
        comparison_value(&missing_duplicate, "")
    );
    let mut nested_order = sample.clone();
    nested_order["plan"]["effects"][0]["values"] = json!([2, 1]);
    assert_ne!(
        comparison_value(&sample, ""),
        comparison_value(&nested_order, "")
    );
    let mut other_order = sample.clone();
    other_order["plan"]["other"] = json!([2, 1]);
    assert_ne!(
        comparison_value(&sample, ""),
        comparison_value(&other_order, "")
    );

    let root = baseline::golden_root().join("plan-json");
    let manifest: Json =
        serde_json::from_slice(&std::fs::read(root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["header"]["main_commit"], baseline::MAIN);
    let entries = manifest["entries"].as_array().unwrap();
    assert_eq!(
        fixture_directories(),
        entries
            .iter()
            .map(|e| e["fixture"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    let mut failures = Vec::new();
    for entry in entries {
        let name = entry["fixture"].as_str().unwrap();
        eprintln!("compare plan fixture: {name}");
        match document(name) {
            Ok(actual) => {
                let Some(file) = entry["file"].as_str() else {
                    let evidence = baseline::save_actual(name, actual.as_bytes());
                    failures.push(format!(
                        "{name}: expected error, got document; {}",
                        evidence.display()
                    ));
                    continue;
                };
                let golden = std::fs::read_to_string(root.join(file)).unwrap();
                let (expected, golden_phcs) = verified_phcs(&golden, name);
                let (actual, candidate_phcs) = verified_phcs(&actual, name);
                baseline::assert_json_eq(
                    &entry["verified_phcs"],
                    &golden_phcs,
                    &format!("{name}: golden PHCs"),
                );
                baseline::assert_json_eq(
                    &golden_phcs,
                    &candidate_phcs,
                    &format!("{name}: candidate PHCs"),
                );
                let expected = comparison_value(&serde_json::from_str(&expected).unwrap(), "");
                let candidate = comparison_value(&serde_json::from_str(&actual).unwrap(), "");
                if let Some(field) = baseline::json_difference(&expected, &candidate) {
                    let evidence = baseline::save_actual(name, actual.as_bytes());
                    failures.push(format!(
                        "{name}: JSON mismatch after Phase 0 ordering comparison: {field}; actual {}",
                        evidence.display()
                    ));
                }
            }
            Err(actual) => {
                if let Some(diff) = baseline::json_difference(
                    &comparison_value(&entry["expected_error"], ""),
                    &comparison_value(&actual, ""),
                ) {
                    failures.push(format!("{name}: {diff}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} plan mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
