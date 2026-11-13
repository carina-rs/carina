//! Keep this source unchanged when comparing #4400 PR heads.
//!
//! The ignored test accepts CARINA_IDENTITY_PROBE_ROOT (the infra checkout),
//! CARINA_IDENTITY_PROBE_PROVIDERS (offline artifacts), and
//! CARINA_IDENTITY_PROBE_OUTPUT (one dump per directory). Real-directory runs
//! write SHA-256 plus byte length for identity and secret bytes, never raw bytes.
//! Files are private (0600 on Unix). No provider runtime is initialized.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use carina_cli::identity_probe::{
    OfflineProviders, ProbeError, RedactionMode, probe_directory, probe_real_directories,
};
use carina_core::EnvironmentSnapshot;
use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderResult, ProviderSession, ProviderValidationInputs,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

struct ProbeFactory;

impl ProbeFactory {
    fn config_schema() -> ValidatedConfigSchema {
        ValidatedConfigSchema::validate(BTreeMap::from([(
            "region".to_owned(),
            AttributeType::string(),
        )]))
        .unwrap()
    }
}

impl ProviderFactory for ProbeFactory {
    fn name(&self) -> &str {
        "probe"
    }

    fn display_name(&self) -> &str {
        "Offline identity probe"
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }

    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        panic!("the identity probe must never initialize a provider")
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let string = |name| AttributeSchema::optional(name, AttributeType::string()).build();
        vec![
            ResourceSchema::new("Source")
                .attribute(string("name"))
                .attribute(
                    AttributeSchema::optional(
                        "names",
                        AttributeType::list(AttributeType::string()),
                    )
                    .build(),
                ),
            ResourceSchema::new("Declared").attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            ),
            ResourceSchema::new("Chained").attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            ),
            ResourceSchema::new("CreateOnly").attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            ),
            ResourceSchema::new("Mutable").attribute(string("name")),
            ResourceSchema::new("Secrets")
                .attribute(string("literal"))
                .attribute(string("known"))
                .attribute(string("unknown")),
        ]
        .into_iter()
        .map(|schema| schema.validate().unwrap())
        .collect()
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Self::config_schema(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}

fn synthetic() -> OfflineProviders {
    OfflineProviders::Factories(vec![Box::new(ProbeFactory)])
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/identity_probe")
}

fn probe(
    directory: &Path,
    providers: OfflineProviders,
) -> Result<String, carina_cli::identity_probe::ProbeError> {
    let mode = match providers {
        OfflineProviders::Factories(factories) => RedactionMode::RawSynthetic(factories),
        providers @ OfflineProviders::ArtifactDirectory(_) => RedactionMode::Sha256(providers),
    };
    probe_directory(directory, &EnvironmentSnapshot::empty(), mode).map(legacy_dump)
}

#[test]
fn offline_identity_inputs_snapshot() {
    let dump = probe(&fixture(), synthetic()).expect("complete offline probe");
    insta::assert_snapshot!(dump);
}

#[test]
fn offline_identity_dump_is_deterministic() {
    let first = probe(&fixture(), synthetic()).unwrap();
    let second = probe(&fixture(), synthetic()).unwrap();
    assert_eq!(first, second);
}

fn copy_fixture(destination: &Path) {
    fn copy(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.path().is_dir() {
                copy(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    copy(&fixture(), destination);
}

#[test]
fn temporary_root_paths_do_not_change_the_dump() {
    let copied = tempfile::tempdir().unwrap();
    copy_fixture(copied.path());
    assert_eq!(
        probe(&fixture(), synthetic()).unwrap(),
        probe(copied.path(), synthetic()).unwrap(),
    );
    assert!(
        !copied
            .path()
            .join("must-not-read-or-write.state.json")
            .exists()
    );
}

#[test]
fn offline_identity_probe_rejects_incomplete_expansion() {
    let copied = tempfile::tempdir().unwrap();
    copy_fixture(copied.path());
    fs::write(
        copied.path().join("deferred.crn"),
        "for name in unknown_b.names { probe.Declared { name = name } }\n",
    )
    .unwrap();
    let result = probe(copied.path(), synthetic());
    expect_incomplete(result, "deferred expansion remains");
}

#[test]
fn offline_identity_probe_rejects_empty_directories_and_missing_schemas() {
    let empty = tempfile::tempdir().unwrap();
    expect_incomplete(
        probe(empty.path(), synthetic()),
        "zero resources enumerated",
    );
    expect_incomplete(
        probe(&fixture(), OfflineProviders::Factories(Vec::new())),
        "missing schema for probe.Declared.<pending>",
    );
    expect_incomplete(
        probe(
            &fixture(),
            OfflineProviders::ArtifactDirectory(empty.path().to_owned()),
        ),
        "no offline provider schemas loaded",
    );
    let missing = empty.path().join("not-created");
    expect_incomplete(
        probe(
            &fixture(),
            OfflineProviders::ArtifactDirectory(missing.clone()),
        ),
        &format!("missing provider artifact directory: {}", missing.display()),
    );
}

#[test]
fn offline_identity_probe_uses_only_the_supplied_environment() {
    let copied = tempfile::tempdir().unwrap();
    copy_fixture(copied.path());
    let sources = copied.path().join("sources.crn");
    let source = fs::read_to_string(&sources)
        .unwrap()
        .replace("\"known-a\"", "env(\"PROBE_KNOWN_NAME\")");
    fs::write(sources, source).unwrap();
    match probe(copied.path(), synthetic()) {
        Err(ProbeError::Application(error)) => {
            assert!(
                error
                    .to_string()
                    .contains("environment variable 'PROBE_KNOWN_NAME' is not set"),
                "{error}"
            )
        }
        result => panic!("expected the missing environment variable error, got {result:?}"),
    }
    let environment =
        EnvironmentSnapshot::new([("PROBE_KNOWN_NAME".to_owned(), "env-known".into())]);
    let OfflineProviders::Factories(factories) = synthetic() else {
        unreachable!()
    };
    let dump = probe_directory(
        copied.path(),
        &environment,
        RedactionMode::RawSynthetic(factories),
    )
    .unwrap();
    let dump: serde_json::Value = serde_json::from_str(&dump).unwrap();
    let secrets = dump["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["resource_id"]["identity"] == "secrets")
        .unwrap();
    let known = secrets["secret_material"]
        .as_array()
        .unwrap()
        .iter()
        .find(|secret| secret["path"] == "known")
        .unwrap();
    assert_eq!(
        known["inputs"][0]["writes"][0]["hex"],
        "22656e762d6b6e6f776e22"
    );
}

#[test]
#[ignore = "explicit infra and artifacts required; writes only SHA-256 and lengths, never raw identity or secret bytes"]
fn real_directory_identity_probe() {
    let required = |name| PathBuf::from(std::env::var_os(name).expect(name));
    let root = required("CARINA_IDENTITY_PROBE_ROOT");
    let artifacts = required("CARINA_IDENTITY_PROBE_PROVIDERS");
    let output = required("CARINA_IDENTITY_PROBE_OUTPUT");
    // This ignored test is the executable boundary for a local probe run.
    #[expect(clippy::disallowed_methods)]
    let environment = EnvironmentSnapshot::capture_process();
    probe_real_directories(&root, &artifacts, &output, &environment)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn real_mode_fingerprints_all_recorded_bytes_without_secret_literals() {
    use carina_cli::identity_probe::RedactionMode;
    let dump = probe_directory(
        &fixture(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::Sha256(synthetic()),
    )
    .unwrap();
    for plaintext in [
        "literal-secret",
        "known-a",
        "legacy-literal",
        "offline-region",
        "fixed-prefix",
    ] {
        assert!(
            !dump.contains(plaintext),
            "real-mode dump contains {plaintext:?}"
        );
        let encoded: String = plaintext
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(
            !dump.contains(&encoded),
            "real-mode dump contains hex plaintext"
        );
    }
    assert!(!dump.contains("\"hex\""));
    assert!(dump.contains("\"sha256\""));
    assert!(dump.contains("\"length\""));
}

#[test]
fn module_outside_the_root_has_relative_declarations_and_contexts() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    copy_fixture(&project);
    fs::rename(project.join("child"), temporary.path().join("external")).unwrap();
    let main = fs::read_to_string(project.join("main.crn"))
        .unwrap()
        .replace("./child", "../external");
    fs::write(project.join("main.crn"), main).unwrap();
    let dump = probe(&project, synthetic()).unwrap();
    assert!(!dump.contains(&temporary.path().display().to_string()));
    assert!(dump.contains("../external/main.crn"));
}

#[test]
fn module_loop_instances_keep_distinct_system_resource_owners() {
    let temporary = tempfile::tempdir().unwrap();
    copy_fixture(temporary.path());
    let main = temporary.path().join("main.crn");
    let mut source = fs::read_to_string(&main).unwrap();
    source.push_str(
        "\nfor name in ['module-loop-first', 'module-loop-second'] { child { name = name } }\n",
    );
    fs::write(main, source).unwrap();
    let OfflineProviders::Factories(factories) = synthetic() else {
        unreachable!()
    };
    let dump = probe_directory(
        temporary.path(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::RawSynthetic(factories),
    )
    .unwrap();
    let dump: serde_json::Value = serde_json::from_str(&dump).unwrap();
    let instances: Vec<_> = dump["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|resource| {
            !resource["expansion"]["instances"]
                .as_array()
                .unwrap()
                .is_empty()
                && resource["expansion"]["loop"]
                    .as_object()
                    .is_some_and(|fields| !fields.is_empty())
        })
        .collect();
    assert_eq!(instances.len(), 2);
    assert_ne!(instances[0]["address"], instances[1]["address"]);
    for resource in instances {
        assert!(
            resource["hash_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|input| input["boundary"] == "declared-identity")
        );
    }
}

#[test]
fn nested_modules_are_associated_by_the_full_call_ancestry() {
    let temporary = tempfile::tempdir().unwrap();
    copy_fixture(temporary.path());
    fs::create_dir_all(temporary.path().join("child/nested")).unwrap();
    fs::write(
        temporary.path().join("child/nested/main.crn"),
        "arguments { name: String }\nprobe.Declared { name = name }\n",
    )
    .unwrap();
    fs::write(
        temporary.path().join("child/nesting.crn"),
        "let nested = use { source = './nested' }\nnested { name = \"${name}-nested\" }\n",
    )
    .unwrap();
    let OfflineProviders::Factories(factories) = synthetic() else {
        unreachable!()
    };
    let dump = probe_directory(
        temporary.path(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::RawSynthetic(factories),
    )
    .unwrap();
    let dump: serde_json::Value = serde_json::from_str(&dump).unwrap();
    let nested: Vec<_> = dump["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|resource| resource["declaration"]["file"] == "child/nested/main.crn")
        .collect();
    assert_eq!(nested.len(), 2);
    for resource in nested {
        assert_eq!(
            resource["expansion"]["instances"].as_array().unwrap().len(),
            2
        );
        assert!(
            resource["hash_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|input| input["boundary"] == "simhash"
                    && input["context"].as_array().unwrap().len() == 2)
        );
    }
}

#[test]
fn fingerprint_mode_hides_values_resolved_from_environment() {
    let temporary = tempfile::tempdir().unwrap();
    copy_fixture(temporary.path());
    let sources = temporary.path().join("sources.crn");
    let source = fs::read_to_string(&sources)
        .unwrap()
        .replace("\"known-a\"", "env(\"PROBE_NAME\")");
    fs::write(sources, source).unwrap();
    let environment =
        EnvironmentSnapshot::new([("PROBE_NAME".to_owned(), "sensitive-env-value".into())]);
    let dump = probe_directory(
        temporary.path(),
        &environment,
        RedactionMode::Sha256(synthetic()),
    )
    .unwrap();
    assert!(!dump.contains("sensitive-env-value"));
    let encoded: String = b"sensitive-env-value"
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert!(!dump.contains(&encoded));
    assert!(!dump.contains("\"hex\""));
}

#[test]
fn empty_simhash_records_its_empty_input_and_result() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(
        temporary.path().join("providers.crn"),
        "provider probe {}\n",
    )
    .unwrap();
    fs::write(temporary.path().join("main.crn"), "probe.Mutable {}\n").unwrap();
    let OfflineProviders::Factories(factories) = synthetic() else {
        unreachable!()
    };
    let dump = probe_directory(
        temporary.path(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::RawSynthetic(factories),
    )
    .unwrap();
    let dump: serde_json::Value = serde_json::from_str(&dump).unwrap();
    let inputs = dump["resources"][0]["hash_inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["boundary"], "simhash");
    assert_eq!(inputs[0]["feature_count"], 0);
    assert_eq!(inputs[0]["writes"], serde_json::json!([]));
    assert_eq!(inputs[0]["output"]["hex"], "0000000000000000");
}

// Retain the original byte/address golden without editing its expectation.
// This projection discards only v2 additions and restores v1 presentation;
// the separate v2 golden below pins actual call order, duplicates and finishes.
fn legacy_dump(dump: String) -> String {
    use serde_json::{Value, json};
    fn input(input: &mut Value, ordinal: usize) {
        let boundary = input["boundary"].as_str().unwrap().to_owned();
        let object = input.as_object_mut().unwrap();
        object.remove("finishes");
        object.remove("feature_count");
        let output = object["output"].get("hex").cloned().unwrap_or(Value::Null);
        object.insert(
            "output".to_owned(),
            if matches!(boundary.as_str(), "create-only" | "simhash-feature") {
                Value::Null
            } else {
                output
            },
        );
        if object["context"]
            .as_array()
            .unwrap()
            .iter()
            .any(|context| context.as_str().unwrap().starts_with("resource:"))
        {
            object.insert("context".to_owned(), json!([format!("resource:{ordinal}")]));
        }
    }
    let mut dump: Value = serde_json::from_str(&dump).unwrap();
    dump["format"] = json!("carina-identity-probe-v1");
    dump.as_object_mut().unwrap().remove("redaction");
    dump.as_object_mut().unwrap().remove("unattributed");
    for resource in dump["resources"].as_array_mut().unwrap() {
        let ordinal = resource["ordinal"].as_u64().unwrap() as usize;
        let inputs = resource["hash_inputs"].as_array_mut().unwrap();
        inputs.retain(|input| input["boundary"] != "simhash");
        for recorded in inputs.iter_mut() {
            input(recorded, ordinal);
        }
        inputs.sort_by_key(Value::to_string);
        for secret in resource["secret_material"].as_array_mut().unwrap() {
            if let Some(inputs) = secret.get_mut("inputs").and_then(Value::as_array_mut) {
                for recorded in inputs {
                    input(recorded, ordinal);
                }
            }
        }
    }
    format!("{}\n", serde_json::to_string_pretty(&dump).unwrap())
}

#[test]
fn offline_identity_ordered_observations_snapshot() {
    let raw = || {
        let OfflineProviders::Factories(factories) = synthetic() else {
            unreachable!()
        };
        probe_directory(
            &fixture(),
            &EnvironmentSnapshot::empty(),
            RedactionMode::RawSynthetic(factories),
        )
        .unwrap()
    };
    let first = raw();
    assert_eq!(first, raw(), "the entire v2 dump must be deterministic");
    let observations: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(observations["format"], "carina-identity-probe-v2");
    assert_eq!(observations["redaction"], "raw-synthetic");
    let unattributed = observations["unattributed"].as_array().unwrap();
    assert!(
        !unattributed.is_empty(),
        "preparatory loop observations must remain explicit"
    );
    assert!(unattributed.iter().all(|input| input["reason"]
        == "preparatory loop template identity not retained by the expanded graph"));
    let dump = serde_json::to_string_pretty(&observations["resources"]).unwrap();
    insta::assert_snapshot!(dump);
}

fn expect_incomplete(result: Result<String, ProbeError>, expected: &str) {
    match result {
        Err(ProbeError::Incomplete(message)) => assert_eq!(message, expected),
        result => panic!("expected Incomplete({expected:?}), got {result:?}"),
    }
}
