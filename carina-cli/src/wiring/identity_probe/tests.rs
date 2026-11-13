use super::*;
use carina_core::identity_recording::{HashFinish, HashWrite, InputOwner};

fn complete_dump(
    prepared: PreparedProbe,
    inputs: Vec<HashInput>,
    paths: &SourcePaths,
    disclosure: Disclosure,
) -> Result<String, ProbeError> {
    super::finish_dump(prepared, inputs, paths, disclosure)?
        .complete(None)
        .map(|outcome| outcome.dump)
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
            dump: object([
                ("ordinal", ordinal.into()),
                ("address", Disclosure::raw().opaque(address)),
                ("secret_material", array([])),
                (
                    "declaration",
                    object([
                        ("file", Disclosure::raw().fixed("fixture.crn")),
                        ("start_byte", (ordinal * 32).into()),
                    ]),
                ),
            ]),
            address: format!("fixture.crn:{}", ordinal * 32),
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
        finishes: vec![HashFinish {
            operation: "finish",
            after_writes: 1,
            output: vec![byte],
        }],
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
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::raw(),
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
            resources: resources(),
            ..PreparedProbe::default()
        },
        Vec::new(),
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::raw(),
    )
    .unwrap_err();
    match error {
        ProbeError::Incomplete(message) => assert!(
            message
                .to_string()
                .starts_with("missing identity hash inputs for fixture.crn:0"),
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
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::raw(),
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
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::raw(),
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
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::raw(),
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
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::raw(),
    )
    .unwrap_err();
    match error {
        ProbeError::Incomplete(message) => assert!(
            message
                .to_string()
                .starts_with("missing module prefix hash inputs for fixture.crn:0"),
            "{message}"
        ),
        error => panic!("wrong probe error: {error:?}"),
    }
}

#[test]
fn directory_walk_collects_failures_and_writes_every_success() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("infra");
    let directories: Vec<_> = [
        "aws/management/bad-first",
        "aws/management/good",
        "aws/management/bad-last",
        "aws/management/deferred",
        "envs/registry/example/good",
        "envs/registry/example/empty",
    ]
    .into_iter()
    .map(|relative| {
        let directory = root.join(relative);
        std::fs::create_dir_all(&directory).unwrap();
        if !relative.ends_with("empty") {
            std::fs::write(directory.join("main.crn"), "fixture").unwrap();
        }
        directory
    })
    .collect();
    let output = temporary.path().join("dumps");
    let error = write_directory_dumps(&root, &directories, &output, |directory| {
        let name = directory.file_name().unwrap().to_str().unwrap();
        match name {
            "bad-first" => Ok(ProbeDocument {
                deferred: Vec::new(),
                dump: "{\"partial-canary\":true}\n".into(),
                missing: vec!["missing fixture observation".into()],
            }),
            "bad-last" => Err(ProbeError::incomplete("fixture failure")),
            "deferred" => Ok(ProbeDocument {
                deferred: vec![DeferredSite { file: Disclosure::raw().fixed("main.crn"), span: 10..20 }],
                dump: "{\"status\":{\"kind\":\"partial\",\"deferred\":[{\"file\":\"main.crn\",\"span\":{\"start\":10,\"end\":20}}]}}\n".into(),
                missing: Vec::new(),
            }),
            "empty" => panic!("empty directory must be skipped before probing"),
            _ => Ok(ProbeDocument {
                deferred: Vec::new(),
                dump: "{\"redaction\":\"sha256\"}\n".into(),
                missing: Vec::new(),
            }),
        }
    })
    .unwrap_err();
    match &error {
        ProbeError::DirectoryFailures(failures) => {
            assert_eq!(failures.failures().count(), 2);
            assert!(failures.report().results().iter().any(|result| matches!(&result.status, DirectoryStatus::Partial { deferred } if deferred == &[DeferredSite { file: Disclosure::raw().fixed("main.crn"), span: 10..20 }])));
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
    assert!(
        output
            .join("aws/management/deferred/identity.json")
            .exists()
    );
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
    let stale = write_directory_dumps(&root, &directories, &output, |_| {
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
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::raw(),
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
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(&root).unwrap(),
        Disclosure::raw(),
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
    assert_eq!(producer_inputs[0].owner, InputOwner::PayloadHash);
    let resources = resources();
    let mut inputs = vec![input(&resources[0], 1), input(&resources[1], 2)];
    inputs.extend(producer_inputs);
    let dump = complete_dump(
        PreparedProbe {
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Sha256,
    )
    .unwrap();
    let document: Json = serde_json::from_str(&dump).unwrap();
    assert!(document.get("secret_producers").is_none());
    assert_eq!(document["unattributed"].as_array().unwrap().len(), 1);
    assert_eq!(
        document["unattributed"][0]["reason"],
        "production secret payload hash outside a resource identity pass"
    );
    assert_eq!(
        document["unattributed"][0]["input"]["context"],
        json!(["secret-payload-hash"])
    );
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
        &SecretPath(vec![SecretPathPart::Attribute(
            Disclosure::Sha256.attribute(&schema, "settings"),
        )]),
        Some(schema.effective_attribute("settings").unwrap().value_type()),
        &mut observations,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Sha256,
    )
    .unwrap();
    assert_eq!(
        observations.len(),
        1,
        "the production payload producer must be observed"
    );
    for observation in &observations {
        let observation = serde_json::to_value(observation).unwrap();
        assert_eq!(observation["path"][0], json!({"attribute": "settings"}));
        assert_eq!(observation["path"][1], json!({"attribute": "public_field"}));
        assert_eq!(observation["path"][2]["map_key"]["length"], 19);
    }
    let dump = serde_json::to_string(&observations).unwrap();
    assert!(!dump.contains("private-path-canary") && !dump.contains("value-canary"));
}

#[test]
fn repeated_simhash_computations_collapse_as_units_and_distinct_order_is_kept() {
    let resources = resources();
    let mut simhash = input(&resources[0], 0);
    simhash.boundary = "simhash";
    simhash.feature_count = Some(2);
    let mut first = input(&resources[0], 1);
    first.boundary = "simhash-feature";
    let mut second = input(&resources[0], 2);
    second.boundary = "simhash-feature";
    let repeated = vec![
        simhash.clone(),
        first.clone(),
        second.clone(),
        simhash,
        first,
        second,
    ];
    assert_eq!(collapse_observations(&repeated).len(), 3);
    let distinct = vec![
        input(&resources[0], 1),
        input(&resources[0], 2),
        input(&resources[0], 1),
        input(&resources[0], 2),
    ];
    assert_eq!(collapse_observations(&distinct).len(), 4);
}

#[test]
fn application_error_reporting_does_not_quote_authored_values() {
    let error = ProbeError::from(AppError::Validation("AUTHORED_PRIVATE_CANARY".to_owned()));
    let message = DirectoryFailures(DirectoryReport(vec![DirectoryResult {
        directory: PathBuf::from("fixture"),
        status: DirectoryStatus::Failed(error),
    }]))
    .to_string();
    assert!(!message.contains("AUTHORED_PRIVATE_CANARY"));
    assert!(message.contains("Application"));
}

#[test]
fn complete_addresses_are_fingerprinted_without_a_recorder() {
    for address in [
        "named.private_key",
        "_for0['private.key']",
        "module.instance.child",
    ] {
        assert_eq!(
            serde_json::to_value(Disclosure::Sha256.opaque(address)).unwrap(),
            serde_json::to_value(identity_recording::fingerprint(address.as_bytes())).unwrap()
        );
        assert_eq!(
            serde_json::to_value(Disclosure::raw().opaque(address)).unwrap(),
            json!(address)
        );
    }
}

#[test]
fn application_debug_and_status_never_quote_authored_values_but_retain_the_source() {
    use std::error::Error;
    let error = ProbeError::from(AppError::Validation("ERROR_PRIVATE_CANARY".into()));
    assert_eq!(format!("{error:?}"), "Application");
    assert_eq!(error.to_string(), "Application");
    assert!(
        error
            .source()
            .unwrap()
            .to_string()
            .contains("ERROR_PRIVATE_CANARY")
    );
    let status = DirectoryStatus::Failed(error);
    assert!(!format!("{status:?}").contains("ERROR_PRIVATE_CANARY"));
    assert!(!status.to_string().contains("ERROR_PRIVATE_CANARY"));
}

#[test]
fn typed_provenance_string_disclosure_does_not_depend_on_json_field_names() {
    let parsed = carina_core::parser::parse_file(
        "let keys = { 'private-provenance-key' = 'one' }\nfor k, v in keys { probe.Declared { name = v } }\n",
        Path::new("provenance.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let origin = parsed.resources[0]
        .loop_provenance
        .display_origin()
        .unwrap();
    let (_, legacy, _) = origin.recording_parts();
    let value = Disclosure::Sha256.opaque(legacy);
    let renamed = json!({"future_provenance_identity": value});
    assert_eq!(
        renamed["future_provenance_identity"],
        serde_json::to_value(identity_recording::fingerprint(legacy.as_bytes())).unwrap()
    );
    let rendered = SourcePaths::new(Path::new("."))
        .unwrap()
        .loop_provenance(&parsed.resources[0].loop_provenance, Disclosure::Sha256);
    let rendered = serde_json::to_value(rendered).unwrap();
    assert_eq!(
        rendered["direct"]["legacy"],
        renamed["future_provenance_identity"]
    );
    let text = serde_json::to_string(&rendered).unwrap();
    assert!(!text.contains("private-provenance-key"));
}

#[test]
fn missing_loop_inputs_do_not_disclose_the_identity_in_errors_or_statuses() {
    let mut resources = resources();
    resources[0]
        .loop_identities
        .push(ResourceIdentity::try_from("PRIVATE_LOOP_IDENTITY_CANARY".to_owned()).unwrap());
    let inputs = vec![input(&resources[0], 1), input(&resources[1], 2)];
    let error = complete_dump(
        PreparedProbe {
            resources,
            ..PreparedProbe::default()
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Sha256,
    )
    .unwrap_err();
    for message in [error.to_string(), format!("{error:?}")] {
        assert!(!message.contains("PRIVATE_LOOP_IDENTITY_CANARY"));
        assert!(message.contains("fixture.crn:0"));
    }
    let status = DirectoryStatus::Failed(error);
    assert!(!status.to_string().contains("PRIVATE_LOOP_IDENTITY_CANARY"));
    assert!(!format!("{status:?}").contains("PRIVATE_LOOP_IDENTITY_CANARY"));
}

#[test]
fn typed_dump_nodes_disclose_only_static_schema_or_source_path_text() {
    let paths = SourcePaths::new(Path::new(".")).unwrap();
    let outside = paths.relative(
        &std::env::current_dir()
            .unwrap()
            .join("../PRIVATE_PATH_CANARY/main.crn"),
    );
    let mut registry = carina_core::schema::SchemaRegistry::new();
    registry.insert(
        "probe",
        carina_core::schema::ResourceSchema::new("Checked")
            .attribute(
                carina_core::schema::AttributeSchema::optional(
                    "declared",
                    carina_core::schema::AttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let schema = registry
        .get(
            "probe",
            "Checked",
            carina_core::schema::SchemaKind::Resource,
        )
        .unwrap();
    let (provider, resource_type) = Disclosure::Sha256.schema_names(schema);
    let document = object([
        ("literal", Disclosure::Sha256.fixed("fixed")),
        ("outside", Disclosure::Sha256.path(&outside)),
        ("provider", provider),
        ("type", resource_type),
        (
            "declared",
            Disclosure::Sha256.attribute(schema.schema(), "declared"),
        ),
        (
            "undeclared",
            Disclosure::Sha256.attribute(schema.schema(), "PRIVATE_ATTRIBUTE_CANARY"),
        ),
        ("container", array([true.into(), 7_usize.into()])),
    ]);
    let text = serde_json::to_string(&document).unwrap();
    assert!(!text.contains("PRIVATE_PATH_CANARY"));
    assert!(!text.contains("PRIVATE_ATTRIBUTE_CANARY"));
    let value: Json = serde_json::from_str(&text).unwrap();
    assert_eq!(value["outside"]["kind"], "outside-root");
    assert_eq!(value["declared"], "declared");
    assert!(value["undeclared"]["sha256"].is_string());
}
