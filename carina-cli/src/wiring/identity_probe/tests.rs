use super::*;
use carina_core::identity_recording::{HashWrite, InputOwner};

fn complete_dump(
    prepared: PreparedProbe,
    inputs: Vec<HashInput>,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<String, ProbeError> {
    super::finish_dump(prepared, inputs, paths, disclosure)?.complete(None)
}

fn resources() -> Vec<PreparedResource> {
    let parsed = carina_core::parser::parse_file(
        "probe.Declared { name = 'first' }\nprobe.Declared { name = 'second' }\n",
        Path::new("fixture.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    parsed
        .resources
        .iter()
        .zip(["first", "second"])
        .enumerate()
        .map(|(ordinal, (resource, address))| PreparedResource {
            dump: json!({ "ordinal": ordinal, "address": address, "secret_material": [] }),
            key: identity_recording::resource_key(resource).unwrap(),
            loop_identities: Vec::new(),
            requires_identity: true,
            required_modules: Vec::new(),
        })
        .collect()
}

fn input(resource: &PreparedResource, byte: u8) -> HashInput {
    HashInput {
        boundary: "declared-identity",
        owner: InputOwner::Resource(resource.key.clone()),
        context: Vec::new(),
        writes: vec![HashWrite {
            operation: "update",
            bytes: vec![byte],
        }],
        output: Some(vec![byte]),
        finishes: Vec::new(),
        feature_count: None,
    }
}

#[test]
fn reordered_inputs_follow_system_ids_and_keep_distinct_call_order() {
    let resources = resources();
    let inputs = vec![
        input(&resources[1], 9),
        input(&resources[0], 2),
        input(&resources[0], 1),
        input(&resources[0], 1),
    ];
    let dump = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap();
    let dump: Json = serde_json::from_str(&dump).unwrap();
    let first = dump["resources"][0]["hash_inputs"].as_array().unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["writes"][0]["hex"], "02");
    assert_eq!(first[1]["writes"][0]["hex"], "01");
    assert_eq!(
        dump["resources"][1]["hash_inputs"][0]["writes"][0]["hex"],
        "09"
    );
    assert_eq!(dump["unattributed"], json!([]));
}

#[test]
fn missing_or_off_thread_identity_recording_is_incomplete() {
    let error = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources: resources(),
            compositions: Vec::new(),
        },
        Vec::new(),
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap_err();
    match error {
        ProbeError::Incomplete(message) => assert!(
            message
                .to_string()
                .starts_with("missing identity hash inputs for resource ordinal 0"),
            "{message}"
        ),
        error => panic!("wrong probe error: {error:?}"),
    }
}

#[test]
fn foreign_inputs_are_reported_and_make_the_probe_incomplete() {
    let resources = resources();
    let mut foreign = input(&resources[0], 7);
    foreign.owner = InputOwner::Unattributed;
    let inputs = vec![input(&resources[0], 1), input(&resources[1], 2), foreign];
    let document = super::finish_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap();
    assert_eq!(document.missing, ["unattributed hash inputs"]);
    let dump: Json = serde_json::from_str(&document.dump).unwrap();
    assert_eq!(dump["unattributed"].as_array().unwrap().len(), 1);
    assert_eq!(dump["unattributed"][0]["input"]["writes"][0]["hex"], "07");
    let error = document.complete(None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "incomplete offline identity probe: unattributed hash inputs"
    );
    assert!(!format!("{error:?}").contains("\"unattributed\""));
}

#[test]
fn observed_simhash_result_cannot_hide_missing_feature_inputs() {
    let resources = resources();
    let mut aggregate = input(&resources[0], 0);
    aggregate.boundary = "simhash";
    aggregate.writes.clear();
    aggregate.feature_count = Some(1);
    let inputs = vec![aggregate, input(&resources[1], 2)];
    let error = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap_err();
    match error {
        ProbeError::Incomplete(message) => assert!(
            message
                .to_string()
                .starts_with("incomplete SimHash feature recording: expected Some(1), observed 0"),
            "{message}"
        ),
        error => panic!("wrong probe error: {error:?}"),
    }
}

#[test]
fn loop_association_does_not_depend_on_digest_or_identity_format() {
    let mut resources = resources();
    let identity = ResourceIdentity::try_from("future-loop-encoding".to_owned()).unwrap();
    resources[0].loop_identities.push(identity.clone());
    let mut loop_input = input(&resources[0], 7);
    loop_input.boundary = "loop-system-identity";
    loop_input.owner = InputOwner::Loop {
        identity,
        parent: None,
    };
    let inputs = vec![loop_input, input(&resources[0], 1), input(&resources[1], 2)];
    let dump = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap();
    let dump: Json = serde_json::from_str(&dump).unwrap();
    assert_eq!(
        dump["resources"][0]["hash_inputs"][0]["boundary"],
        "loop-system-identity"
    );
    assert_eq!(dump["unattributed"], json!([]));
}

#[test]
fn missing_anonymous_module_prefix_recording_is_incomplete() {
    let parsed = carina_core::parser::parse_file(
        "let child = use { source = './child' }\nchild {}\n",
        Path::new("module-calls.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let mut resources = resources();
    resources[0].required_modules =
        ModuleKey::prefixes(parsed.module_calls.iter().map(|call| call.id()));
    let inputs = vec![input(&resources[0], 1), input(&resources[1], 2)];
    let error = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap_err();
    match error {
        ProbeError::Incomplete(message) => assert!(
            message
                .to_string()
                .starts_with("missing module prefix hash inputs for resource ordinal 0"),
            "{message}"
        ),
        error => panic!("wrong probe error: {error:?}"),
    }
}

#[test]
fn directory_walk_collects_failures_and_writes_every_success() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("infra");
    for relative in [
        "aws/management/bad-first",
        "aws/management/good",
        "aws/management/bad-last",
        "envs/registry/example/good",
        "envs/registry/example/empty",
    ] {
        let directory = root.join(relative);
        std::fs::create_dir_all(&directory).unwrap();
        if !relative.ends_with("empty") {
            std::fs::write(directory.join("main.crn"), "fixture").unwrap();
        }
    }
    let output = temporary.path().join("dumps");
    let error = write_directory_dumps(&root, &output, |directory| {
        let name = directory.file_name().unwrap().to_str().unwrap();
        match name {
            "bad-first" => Ok(ProbeDocument {
                dump: "{\"partial-canary\":true}\n".into(),
                missing: vec!["missing fixture observation".into()],
            }),
            "bad-last" => Err(ProbeError::incomplete("fixture failure")),
            "empty" => panic!("empty directory must be skipped before probing"),
            _ => Ok(ProbeDocument {
                dump: "{\"redaction\":\"sha256\"}\n".into(),
                missing: Vec::new(),
            }),
        }
    })
    .unwrap_err();
    match &error {
        ProbeError::DirectoryFailures(failures) => {
            assert_eq!(failures.failures().count(), 2);
            assert!(
                failures
                    .report()
                    .results()
                    .iter()
                    .any(|result| result.directory.ends_with("empty")
                        && matches!(result.status, DirectoryStatus::Skipped))
            );
            let partial = &failures
                .report()
                .results()
                .iter()
                .find(|result| result.directory.ends_with("bad-first"))
                .unwrap()
                .status;
            match partial {
                DirectoryStatus::Failed(ProbeError::Incomplete(details)) => {
                    assert_eq!(details.missing, ["missing fixture observation"]);
                    assert_eq!(
                        details.dump_path.as_ref().unwrap(),
                        &output.join("aws/management/bad-first/identity.json")
                    );
                }
                other => panic!("wrong partial result: {other:?}"),
            }
        }
        error => panic!("wrong batch error: {error:?}"),
    }
    let message = error.to_string();
    assert!(!message.contains("partial-canary"));
    assert!(message.contains("partial dump:"));
    assert!(message.contains("bad-first") && message.contains("bad-last"));
    assert_eq!(
        std::fs::read_to_string(output.join("aws/management/bad-first/identity.json")).unwrap(),
        "{\"partial-canary\":true}\n"
    );
    for relative in [
        "aws/management/good/identity.json",
        "envs/registry/example/good/identity.json",
    ] {
        let path = output.join(relative);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"redaction\":\"sha256\"}\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let stale = write_directory_dumps(&root, &output, |_| {
        panic!("must reject non-empty output before probing")
    })
    .unwrap_err();
    match stale {
        ProbeError::Incomplete(details) => assert_eq!(
            details.missing,
            [format!("output root is not empty: {}", output.display())]
        ),
        other => panic!("wrong stale-output error: {other:?}"),
    }
}

#[test]
fn round_two_consecutive_repeats_collapse_per_owner_without_reordering_distinct_inputs() {
    let resources = resources();
    let inputs = vec![
        input(&resources[0], 2),
        input(&resources[1], 9),
        input(&resources[0], 2),
        input(&resources[0], 1),
        input(&resources[0], 1),
        input(&resources[0], 2),
    ];
    let dump = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap();
    let document: Json = serde_json::from_str(&dump).unwrap();
    let writes: Vec<_> = document["resources"][0]["hash_inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|input| input["writes"][0]["hex"].as_str().unwrap())
        .collect();
    assert_eq!(writes, ["02", "01", "02"]);
}

#[test]
fn round_two_nonempty_output_root_is_rejected_before_any_probe() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("infra");
    std::fs::create_dir_all(root.join("aws/management/fixture")).unwrap();
    std::fs::create_dir_all(root.join("envs/registry/test/fixture")).unwrap();
    let output = temporary.path().join("dumps");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("stale.json"), "stale").unwrap();
    let error = write_directory_dumps(&root, &output, |_| {
        panic!("must reject stale output before probing")
    })
    .unwrap_err();
    match error {
        ProbeError::Incomplete(message) => assert!(
            message.to_string().contains("output root is not empty"),
            "{message}"
        ),
        error => panic!("wrong output-root error: {error:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(output.join("stale.json")).unwrap(),
        "stale"
    );
}

#[test]
fn round_two_unattributed_contexts_outside_root_are_relative() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let resources = resources();
    let mut template = input(&resources[0], 7);
    template.owner = InputOwner::Loop {
        identity: ResourceIdentity::try_from("template-loop".to_owned()).unwrap(),
        parent: None,
    };
    template.boundary = "loop-system-identity";
    let calls = carina_core::parser::parse_file(
        "let child = use { source = './child' }\nchild {}\n",
        &temporary.path().join("external/main.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    template.context = vec![InputContext::Module(calls.module_calls[0].id().clone())];
    let inputs = vec![input(&resources[0], 1), input(&resources[1], 2), template];
    let dump = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(&root).unwrap(),
        Disclosure::Raw,
    )
    .unwrap();
    assert!(
        !dump.contains(&temporary.path().display().to_string()),
        "whole document must not contain absolute temporary paths"
    );
    assert!(dump.contains("../external/main.crn"));
}

#[test]
fn production_secret_hashes_outside_resources_have_a_named_owner() {
    let payload: carina_core::resource::SecretPayload<
        carina_core::resource::NoPlaceholder,
        carina_core::resource::AbsoluteBindingAddress,
    > = Box::new(Value::Concrete(ConcreteValue::String(
        "producer-canary".into(),
    )))
    .into();
    let (_, producer_inputs) = identity_recording::record(|| payload.hash(None).unwrap());
    assert_eq!(
        producer_inputs[0].owner,
        InputOwner::SecretProducer(identity_recording::SecretProducer::PayloadHash)
    );
    let resources = resources();
    let mut inputs = vec![input(&resources[0], 1), input(&resources[1], 2)];
    inputs.extend(producer_inputs);
    let dump = complete_dump(
        PreparedProbe {
            missing: Vec::new(),
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Sha256,
    )
    .unwrap();
    let document: Json = serde_json::from_str(&dump).unwrap();
    assert_eq!(
        document["secret_producers"]["secret-payload-hash"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(document["unattributed"], json!([]));
}

#[test]
fn secret_paths_keep_schema_fields_readable_and_fingerprint_only_map_keys() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
    let schema = ResourceSchema::new("SecretRecord")
        .attribute(
            AttributeSchema::optional(
                "settings",
                AttributeType::struct_(
                    "Settings",
                    vec![
                        StructField::required(
                            "public_field",
                            AttributeType::map(AttributeType::string()),
                        )
                        .build(),
                    ],
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let secret = Value::Deferred(DeferredValue::Secret(
        Box::new(Value::Concrete(ConcreteValue::String(
            "value-canary".into(),
        )))
        .into(),
    ));
    let value = Value::Concrete(ConcreteValue::Map(
        [(
            "public_field".to_owned(),
            Value::Concrete(ConcreteValue::Map(
                [("private-path-canary".to_owned(), secret)]
                    .into_iter()
                    .collect(),
            )),
        )]
        .into_iter()
        .collect(),
    ));
    let mut observations = Vec::new();
    observe_secrets(
        &value,
        &SecretPath(vec![SecretPathPart::Attribute("settings".into())]),
        Some(schema.effective_attribute("settings").unwrap().value_type()),
        &mut observations,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Sha256,
    )
    .unwrap();
    assert_eq!(
        observations.len(),
        2,
        "both production producers must be observed"
    );
    for observation in &observations {
        assert_eq!(observation["path"][0], json!({"attribute": "settings"}));
        assert_eq!(observation["path"][1], json!({"attribute": "public_field"}));
        assert_eq!(observation["path"][2]["map_key"]["length"], 19);
    }
    let dump = serde_json::to_string(&observations).unwrap();
    assert!(!dump.contains("private-path-canary") && !dump.contains("value-canary"));
}
