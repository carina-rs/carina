//! Keep this source unchanged when comparing #4400 PR heads.
//!
//! The ignored test accepts CARINA_IDENTITY_PROBE_ROOT (the infra checkout),
//! CARINA_IDENTITY_PROBE_PROVIDERS (offline artifacts), and
//! CARINA_IDENTITY_PROBE_OUTPUT (one dump per directory). No provider is run.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use carina_cli::identity_probe::{OfflineProviders, probe_directory};
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

impl ProviderFactory for ProbeFactory {
    fn name(&self) -> &str {
        "probe"
    }

    fn display_name(&self) -> &str {
        "Offline identity probe"
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::validate(BTreeMap::from([(
            "region".to_owned(),
            AttributeType::string(),
        )]))
        .unwrap()
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
            self.provider_config_attribute_types(),
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
    probe_directory(directory, providers, &EnvironmentSnapshot::empty())
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
    assert!(result.is_err(), "a partial expansion must fail: {result:?}");
}

#[test]
fn offline_identity_probe_rejects_empty_directories_and_missing_schemas() {
    let empty = tempfile::tempdir().unwrap();
    assert!(probe(empty.path(), synthetic()).is_err());
    assert!(probe(&fixture(), OfflineProviders::Factories(Vec::new())).is_err());
    assert!(
        probe(
            &fixture(),
            OfflineProviders::ArtifactDirectory(empty.path().to_owned())
        )
        .is_err()
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
    assert!(probe(copied.path(), synthetic()).is_err());
    let environment =
        EnvironmentSnapshot::new([("PROBE_KNOWN_NAME".to_owned(), "env-known".into())]);
    let dump = probe_directory(copied.path(), synthetic(), &environment).unwrap();
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

fn children(directory: &Path) -> Vec<PathBuf> {
    let mut children: Vec<_> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    children.sort();
    assert!(
        !children.is_empty(),
        "no directories in {}",
        directory.display()
    );
    children
}

#[test]
#[ignore = "requires an explicit infra root and offline provider artifacts"]
fn real_directory_identity_probe() {
    let required = |name| PathBuf::from(std::env::var_os(name).expect(name));
    let root = required("CARINA_IDENTITY_PROBE_ROOT");
    let artifacts = required("CARINA_IDENTITY_PROBE_PROVIDERS");
    let output = required("CARINA_IDENTITY_PROBE_OUTPUT");
    // This ignored test is the executable boundary for a local probe run.
    #[expect(clippy::disallowed_methods)]
    let environment = EnvironmentSnapshot::capture_process();
    let mut directories = children(&root.join("aws/management"));
    for registry in children(&root.join("envs/registry")) {
        directories.extend(children(&registry));
    }
    directories.sort();
    for directory in directories {
        let dump = probe_directory(
            &directory,
            OfflineProviders::ArtifactDirectory(artifacts.clone()),
            &environment,
        )
        .unwrap_or_else(|error| panic!("probe {}: {error}", directory.display()));
        let destination = output.join(directory.strip_prefix(&root).unwrap());
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("identity.json"), dump).unwrap();
    }
}
