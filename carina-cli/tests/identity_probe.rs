//! Keep this source unchanged when comparing #4400 PR heads.
//!
//! The ignored test accepts CARINA_IDENTITY_PROBE_ROOT (the infra checkout),
//! CARINA_IDENTITY_PROBE_PROVIDERS (offline artifacts), and
//! CARINA_IDENTITY_PROBE_OUTPUT (one dump per directory). Real-directory runs
//! write SHA-256 plus byte length for identity and secret bytes, never raw bytes.
//! Fingerprints are unsalted SHA-256 + length: dictionary attacks recover weak
//! secrets and low-entropy keys. Real-directory dumps are sensitive local evidence
//! under 0600;
//! never attach dumps or captured stderr to PRs or CI artifacts. Publish only
//! the comparison verdict. The output root must be fresh and empty.
//! Schema-only installed artifacts are read; provider runtimes, network, AWS and
//! state backends are never used. Only static literals, in-root source paths,
//! and schema-backed names stay readable; other strings are fingerprinted.
//! Consecutive
//! identical computations collapse.
//! The whole v3 snapshot includes unattributed observations with stated reasons.
//! Use the same checkout path for main and candidate: preparatory loop inputs
//! include absolute root bytes. RawSynthetic accepts any directory; keep raw
//! runs limited to generated fixtures. The paired pipeline's own stderr warnings
//! are not controlled by the probe; this test captures worker stderr without
//! echoing it, and reports only its byte count.

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
            ResourceSchema::new("SecretMap").attribute(
                AttributeSchema::optional("values", AttributeType::map(AttributeType::string()))
                    .build(),
            ),
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
    PathBuf::from("tests/fixtures/identity_probe")
}

fn probe(
    directory: &Path,
    providers: OfflineProviders,
) -> Result<String, carina_cli::identity_probe::ProbeError> {
    let mode = match providers {
        OfflineProviders::Factories(factories) => RedactionMode::RawSynthetic(factories),
        providers @ OfflineProviders::ArtifactDirectory(_) => RedactionMode::Sha256(providers),
    };
    probe_directory(directory, &EnvironmentSnapshot::empty(), mode).map(|outcome| outcome.dump)
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
fn temporary_root_paths_preserve_resource_results_and_normalize_all_contexts() {
    let copied = tempfile::tempdir().unwrap();
    copy_fixture(copied.path());
    let original: serde_json::Value =
        serde_json::from_str(&probe(&fixture(), synthetic()).unwrap()).unwrap();
    let relocated: serde_json::Value =
        serde_json::from_str(&probe(copied.path(), synthetic()).unwrap()).unwrap();
    assert_eq!(original["resources"], relocated["resources"]);
    // Unretained preparatory loop hashes genuinely include the source-root
    // bytes. Preserve that evidence, while normalizing every context label.
    let contexts = |document: &serde_json::Value| {
        document["unattributed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["input"]["context"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(contexts(&original), contexts(&relocated));
    assert_no_absolute_paths(&original);
    assert_no_absolute_paths(&relocated);
    assert!(
        !copied
            .path()
            .join("must-not-read-or-write.state.json")
            .exists()
    );
}

#[test]
fn deferred_loop_body_is_covered_when_the_iterable_becomes_known() {
    let copied = tempfile::tempdir().unwrap();
    copy_fixture(copied.path());
    fs::write(
        copied.path().join("iterable.crn"),
        "let names = unknown_b.names\n",
    )
    .unwrap();
    fs::write(
        copied.path().join("deferred.crn"),
        "for name in names { probe.Declared { name = name } }\n",
    )
    .unwrap();
    let outcome = probe_directory(
        copied.path(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::RawSynthetic(vec![Box::new(ProbeFactory)]),
    )
    .unwrap();
    assert_eq!(outcome.deferred.len(), 1);
    assert_eq!(
        serde_json::to_value(&outcome.deferred[0].file).unwrap(),
        "deferred.crn"
    );
    let pending: serde_json::Value =
        serde_json::from_str(&probe(copied.path(), synthetic()).unwrap()).unwrap();
    assert_eq!(pending["status"]["kind"], "partial");
    assert_eq!(pending["status"]["deferred"][0]["file"], "deferred.crn");
    assert!(pending["status"]["deferred"][0]["span"]["start"].is_number());
    assert!(
        pending["resources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|resource| resource["declaration"]["file"] != "deferred.crn")
    );
    fs::write(
        copied.path().join("iterable.crn"),
        "let names = ['body-known-locally']\n",
    )
    .unwrap();
    let expanded: serde_json::Value =
        serde_json::from_str(&probe(copied.path(), synthetic()).unwrap()).unwrap();
    assert_eq!(expanded["status"]["kind"], "ok");
    let body = expanded["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["declaration"]["file"] == "deferred.crn")
        .expect("known iterable must emit the body");
    for boundary in ["declared-identity", "loop-system-identity"] {
        assert!(
            body["hash_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|input| input["boundary"] == boundary)
        );
    }
}

#[test]
fn offline_identity_probe_rejects_empty_directories_and_missing_schemas() {
    let empty = tempfile::tempdir().unwrap();
    assert!(matches!(
        probe(empty.path(), synthetic()),
        Err(ProbeError::NoConfiguration)
    ));
    fs::write(empty.path().join("providers.crn"), "provider probe {}\n").unwrap();
    expect_incomplete(
        probe(empty.path(), synthetic()),
        "zero resources enumerated",
    );
    expect_incomplete(
        probe(&fixture(), OfflineProviders::Factories(Vec::new())),
        "missing schema at main.crn:0",
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
    let dump = probe_directory(
        copied.path(),
        &environment,
        RedactionMode::RawSynthetic(vec![Box::new(ProbeFactory)]),
    )
    .unwrap()
    .dump;
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
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "offline_identity_probe_worker",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    println!("captured pipeline stderr bytes: {}", output.stderr.len());
    print!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "identity probe worker exited {}",
        output.status
    );
}

fn worker_failed_before_probing(class: &'static str) -> ! {
    println!("worker failed before probing: {class}");
    panic!("identity probe worker failed before reporting");
}

#[test]
#[ignore = "internal worker; invoke real_directory_identity_probe to capture stderr"]
fn offline_identity_probe_worker() {
    let prepared = std::panic::catch_unwind(|| {
        let required = |name: &'static str, missing: &'static str| {
            std::env::var_os(name).map(PathBuf::from).ok_or(missing)
        };
        let root = required(
            "CARINA_IDENTITY_PROBE_ROOT",
            "missing CARINA_IDENTITY_PROBE_ROOT",
        )?;
        let artifacts = required(
            "CARINA_IDENTITY_PROBE_PROVIDERS",
            "missing CARINA_IDENTITY_PROBE_PROVIDERS",
        )?;
        let output = required(
            "CARINA_IDENTITY_PROBE_OUTPUT",
            "missing CARINA_IDENTITY_PROBE_OUTPUT",
        )?;
        // This ignored test is the executable boundary for a local probe run.
        #[expect(clippy::disallowed_methods)]
        let environment = EnvironmentSnapshot::capture_process();
        let children = |directory: &Path| {
            let mut paths: Vec<_> = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir())
                .collect();
            paths.sort();
            paths
        };
        let mut directories = children(&root.join("aws/management"));
        for registry in children(&root.join("envs/registry")) {
            directories.extend(children(&registry));
        }
        let result = probe_real_directories(&root, &directories, &artifacts, &output, &environment);
        Ok::<_, &'static str>((root, result))
    });
    let (root, result) = match prepared {
        Ok(Ok(result)) => result,
        Ok(Err(class)) => worker_failed_before_probing(class),
        Err(_) => worker_failed_before_probing("panic"),
    };
    let report = match &result {
        Ok(report) => report,
        Err(ProbeError::DirectoryFailures(failures)) => failures.report(),
        Err(error) => worker_failed_before_probing(match error {
            ProbeError::Application(_) => "Application",
            ProbeError::Io(_) => "Io",
            ProbeError::Json(_) => "Json",
            ProbeError::Incomplete(_) => "Incomplete",
            ProbeError::NoConfiguration => "NoConfiguration",
            ProbeError::DirectoryFailures(_) => "DirectoryFailures",
        }),
    };
    for result in report.results() {
        let directory = result
            .directory
            .strip_prefix(&root)
            .unwrap_or(&result.directory);
        println!("{}: {}", directory.display(), result.status);
    }
    result.unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn real_mode_fingerprints_all_recorded_bytes_without_secret_literals() {
    use carina_cli::identity_probe::RedactionMode;
    let dump = probe_directory(
        &fixture(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::Sha256(synthetic()),
    )
    .unwrap()
    .dump;
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
    assert_no_absolute_paths(&serde_json::from_str(&dump).unwrap());
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
    let dump = probe(temporary.path(), synthetic()).unwrap();
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
    let dump = probe(temporary.path(), synthetic()).unwrap();
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
    .unwrap()
    .dump;
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
    let dump = probe(temporary.path(), synthetic()).unwrap();
    let dump: serde_json::Value = serde_json::from_str(&dump).unwrap();
    let inputs = dump["resources"][0]["hash_inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["boundary"], "simhash");
    assert_eq!(inputs[0]["feature_count"], 0);
    assert_eq!(inputs[0]["writes"], serde_json::json!([]));
    assert_eq!(
        inputs[0]["finishes"][0]["output"]["hex"],
        "0000000000000000"
    );
}

#[test]
fn offline_identity_ordered_observations_snapshot() {
    let first = probe(&fixture(), synthetic()).unwrap();
    assert_eq!(
        first,
        probe(&fixture(), synthetic()).unwrap(),
        "the entire v3 dump must be deterministic"
    );
    let observations: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(observations["format"], "carina-identity-probe-v3");
    assert_eq!(observations["redaction"], "raw-synthetic");
    let unattributed = observations["unattributed"].as_array().unwrap();
    assert!(
        !unattributed.is_empty(),
        "preparatory loop observations must remain explicit"
    );
    assert!(unattributed.iter().all(|input| input["reason"]
        == "preparatory loop template identity not retained by the expanded graph"));
    assert_no_absolute_paths(&observations);
    insta::assert_snapshot!(first);
}

fn expect_incomplete(result: Result<String, ProbeError>, expected: &str) {
    match result {
        Err(ProbeError::Incomplete(details)) => {
            assert_eq!(details.missing.first().map(String::as_str), Some(expected));
            assert!(details.dump_path.is_none());
        }
        result => panic!("expected Incomplete({expected:?}), got {result:?}"),
    }
}

fn assert_no_plaintext_or_hex(dump: &str, private: &str) {
    assert!(
        !dump.contains(private),
        "private key leaked in real-mode metadata"
    );
    let encoded: String = private
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert!(
        !dump.contains(&encoded),
        "hex private key leaked in real-mode metadata"
    );
}

#[test]
fn round_two_fingerprint_mode_hides_environment_map_loop_keys() {
    for private in [
        "environmentLoopKeyCanary",
        "environment-loop-key-CANARY.with space",
    ] {
        // Keep identical module arguments in separate configurations: combining
        // bound and unbound calls would itself produce an identity collision.
        for (loops, expected_count, has_unbound) in [
            (
                r#"
for key, name in keyed { probe.Declared { name = "${name}-anonymous" } }
let named = for key, name in keyed { probe.Declared { name = "${name}-anonymous" } }
for key, name in keyed { child { name = "${name}-module" } }
"#,
                20,
                true,
            ),
            (
                r#"
let other_named = for key, name in keyed { probe.Declared { name = "${name}-anonymous" } }
let module_named = for key, name in keyed { child { name = "${name}-module" } }
"#,
                19,
                false,
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            copy_fixture(temporary.path());
            let environment =
                EnvironmentSnapshot::new([("PROBE_PRIVATE_KEY".to_owned(), private.into())]);
            // Map keys are literals in this branch's grammar. Evaluate env() through
            // the production parser, then use that value to construct the fixture map.
            let config = carina_core::parser::ProviderContext::new(environment.clone());
            let mut parsed = carina_core::parser::parse_file(
                "let key = env('PROBE_PRIVATE_KEY')\n",
                Path::new("environment.crn"),
                &config,
            )
            .unwrap();
            carina_core::parser::resolve_resource_refs(&mut parsed, &config).unwrap();
            let carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(key),
            ) = &parsed.variables["key"]
            else {
                panic!("env must produce a string");
            };
            assert!(key.is_diagnostic_sensitive());
            let key = serde_json::to_string(key).unwrap();
            fs::write(
                temporary.path().join("map-loops.crn"),
                format!(
                    r#"
let environment_key = env('PROBE_PRIVATE_KEY')
let keyed = {{ {key} = 'map-loop-name' }}
{loops}
"#
                ),
            )
            .unwrap();
            let result = probe_directory(
                temporary.path(),
                &environment,
                RedactionMode::Sha256(synthetic()),
            );
            assert_no_plaintext_or_hex(&format!("{result:?}"), private);
            if let Err(error) = &result {
                assert_no_plaintext_or_hex(&error.to_string(), private);
                assert_no_plaintext_or_hex(&format!("{error:?}"), private);
            }
            let outcome = result.expect("all bound and unbound keyed loops must succeed");
            let status = if outcome.deferred.is_empty() {
                carina_cli::identity_probe::DirectoryStatus::Ok
            } else {
                carina_cli::identity_probe::DirectoryStatus::Partial {
                    deferred: outcome.deferred,
                }
            };
            assert_no_plaintext_or_hex(&status.to_string(), private);
            assert_no_plaintext_or_hex(&format!("{status:?}"), private);
            let dump = outcome.dump;
            assert_no_plaintext_or_hex(&dump, private);
            let raw = probe_directory(
                temporary.path(),
                &environment,
                RedactionMode::RawSynthetic(vec![Box::new(ProbeFactory)]),
            )
            .unwrap()
            .dump;
            let raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let resources = raw["resources"].as_array().unwrap();
            assert!(
                resources
                    .iter()
                    .any(|resource| resource["address"].as_str().unwrap().contains(private)),
                "raw address must exercise the key"
            );
            if has_unbound {
                assert!(
                    resources
                        .iter()
                        .flat_map(|resource| resource["hash_inputs"].as_array().unwrap())
                        .flat_map(|input| input["context"].as_array().unwrap())
                        .any(|context| context.as_str().is_some_and(|label| label
                            .starts_with("module:")
                            && label.contains(private))),
                    "raw module expansion must exercise the key (case {expected_count})"
                );
            } else {
                assert!(
                    resources
                        .iter()
                        .flat_map(|resource| resource["expansion"]["instances"].as_array().unwrap())
                        .any(|instance| instance["instance"].as_str().unwrap().contains(private)),
                    "raw bound module instance must exercise the key"
                );
            }
            if has_unbound {
                let origins: Vec<_> = resources
                    .iter()
                    .flat_map(|resource| {
                        let provenance = &resource["expansion"]["loop"];
                        provenance.get("direct").into_iter().chain(
                            provenance["module_prefixes"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|prefix| &prefix["origin"]),
                        )
                    })
                    .collect();
                assert!(
                    origins
                        .iter()
                        .any(|origin| origin["legacy"].as_str().unwrap().contains(private)),
                    "raw legacy address must exercise the key"
                );
                assert!(
                    origins
                        .iter()
                        .any(|origin| origin["kind"]["element"]["value"] == private),
                    "raw loop element must exercise the key"
                );
            }
            let document: serde_json::Value = serde_json::from_str(&dump).unwrap();
            assert_eq!(
                document["resources"].as_array().unwrap().len(),
                expected_count,
                "every keyed loop body must expand"
            );
            assert!(dump.contains("map-loops.crn"));
            assert!(dump.contains("Declared"));
            for resource in document["resources"].as_array().unwrap() {
                assert!(resource["address"]["sha256"].is_string());
                assert!(resource["resource_id"]["identity"]["sha256"].is_string());
            }
        }
    }
}

#[test]
fn round_two_fingerprint_mode_hides_secret_path_map_keys() {
    let temporary = tempfile::tempdir().unwrap();
    copy_fixture(temporary.path());
    fs::write(
        temporary.path().join("secret-map.crn"),
        r#"
let secret_map = probe.SecretMap {
    values = { "private-secret-map-key-CANARY" = secret('map-secret-value') }
}
"#,
    )
    .unwrap();
    let private = "private-secret-map-key-CANARY";
    let environment = EnvironmentSnapshot::new([("PROBE_PRIVATE_KEY".to_owned(), private.into())]);
    let dump = probe_directory(
        temporary.path(),
        &environment,
        RedactionMode::Sha256(synthetic()),
    )
    .unwrap()
    .dump;
    assert_no_plaintext_or_hex(&dump, private);
    assert!(
        dump.contains("values"),
        "schema-declared attributes stay readable"
    );
}

fn assert_no_absolute_paths(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(value) => {
            assert!(
                !Path::new(value).is_absolute(),
                "absolute path in the document"
            );
            assert!(!value.contains(":/"), "absolute path in a context label");
            assert!(
                !value.contains("/Volumes/CargoTargets/"),
                "absolute scratch path in the document"
            );
            assert!(
                !value.contains("/Users/"),
                "absolute workspace path in the document"
            );
        }
        serde_json::Value::Array(values) => {
            for value in values {
                assert_no_absolute_paths(value);
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                assert_no_absolute_paths(&serde_json::json!(key));
                assert_no_absolute_paths(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[test]
fn missing_schema_error_identifies_declaration_without_a_loop_key() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("providers.crn"),
        "provider probe {}\n",
    )
    .unwrap();
    fs::write(directory.path().join("keyed.crn"), "let xs = { 'PRIVATE_LOOP_KEY_CANARY' = 'one' }\nlet items = for key, value in xs { probe.Declared { name = value } }\n").unwrap();
    let error = probe_directory(
        directory.path(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::Sha256(OfflineProviders::Factories(Vec::new())),
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(!message.contains("PRIVATE_LOOP_KEY_CANARY"));
    assert!(message.contains("keyed.crn:"));
}

#[test]
fn sha_paths_outside_root_hide_the_directory_names() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    copy_fixture(&project);
    fs::rename(
        project.join("child"),
        temporary.path().join("PRIVATE_OUTSIDE_DIRECTORY_CANARY"),
    )
    .unwrap();
    let main = fs::read_to_string(project.join("main.crn"))
        .unwrap()
        .replace("./child", "../PRIVATE_OUTSIDE_DIRECTORY_CANARY");
    fs::write(project.join("main.crn"), main).unwrap();
    let real = probe_directory(
        &project,
        &EnvironmentSnapshot::empty(),
        RedactionMode::Sha256(synthetic()),
    )
    .unwrap()
    .dump;
    assert!(!real.contains("PRIVATE_OUTSIDE_DIRECTORY_CANARY"));
    assert!(!real.contains("../"));
    assert!(real.contains("outside-root"));
    let raw = probe(&project, synthetic()).unwrap();
    assert!(raw.contains("../PRIVATE_OUTSIDE_DIRECTORY_CANARY/main.crn"));
}

#[test]
fn undeclared_authored_attribute_names_are_fingerprinted() {
    let temporary = tempfile::tempdir().unwrap();
    copy_fixture(temporary.path());
    fs::write(
        temporary.path().join("unchecked.crn"),
        "let unchecked = probe.Secrets { unchecked_secret_key_CANARY = secret('fixture-value') }\n",
    )
    .unwrap();
    let real = probe_directory(
        temporary.path(),
        &EnvironmentSnapshot::empty(),
        RedactionMode::Sha256(synthetic()),
    )
    .unwrap()
    .dump;
    assert!(!real.contains("unchecked_secret_key_CANARY"));
    let raw = probe(temporary.path(), synthetic()).unwrap();
    assert!(raw.contains("unchecked_secret_key_CANARY"));
}

#[test]
fn real_probe_worker_reports_fixed_setup_failures_and_ignores_inherited_marker() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    fs::create_dir_all(root.join("aws/management")).unwrap();
    fs::create_dir_all(root.join("envs/registry")).unwrap();
    let output = temporary.path().join("PRIVATE_SETUP_PATH_CANARY");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("stale"), "fixture").unwrap();
    for (configured, expected) in [
        (
            false,
            "worker failed before probing: missing CARINA_IDENTITY_PROBE_ROOT",
        ),
        (true, "worker failed before probing: Incomplete"),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "real_directory_identity_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("CARINA_IDENTITY_PROBE_WORKER", "1")
            .env_remove("CARINA_IDENTITY_PROBE_ROOT")
            .env_remove("CARINA_IDENTITY_PROBE_PROVIDERS")
            .env_remove("CARINA_IDENTITY_PROBE_OUTPUT");
        if configured {
            command
                .env("CARINA_IDENTITY_PROBE_ROOT", &root)
                .env("CARINA_IDENTITY_PROBE_PROVIDERS", temporary.path())
                .env("CARINA_IDENTITY_PROBE_OUTPUT", &output);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("captured pipeline stderr bytes:"));
        assert!(stdout.contains(expected), "missing fixed failure class");
        assert!(!stdout.contains("PRIVATE_SETUP_PATH_CANARY"));
        assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE_SETUP_PATH_CANARY"));
    }
}
