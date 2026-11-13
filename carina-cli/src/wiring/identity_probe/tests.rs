use super::*;
use carina_core::identity_recording::{HashWrite, InputOwner};

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
        .map(|(resource, address)| PreparedResource {
            dump: json!({ "address": address, "secret_material": [] }),
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
fn reordered_inputs_follow_system_ids_and_keep_call_order_and_duplicates() {
    let resources = resources();
    let inputs = vec![
        input(&resources[1], 9),
        input(&resources[0], 2),
        input(&resources[0], 1),
        input(&resources[0], 1),
    ];
    let dump = finish_dump(
        PreparedProbe {
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
    assert_eq!(first.len(), 3);
    assert_eq!(first[0]["writes"][0]["hex"], "02");
    assert_eq!(first[1]["writes"][0]["hex"], "01");
    assert_eq!(first[2]["writes"][0]["hex"], "01");
    assert_eq!(
        dump["resources"][1]["hash_inputs"][0]["writes"][0]["hex"],
        "09"
    );
    assert_eq!(dump["unattributed"], json!([]));
}

#[test]
fn missing_or_off_thread_identity_recording_is_incomplete() {
    let error = finish_dump(
        PreparedProbe {
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
            message.starts_with("missing identity hash inputs for first"),
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
    let error = finish_dump(
        PreparedProbe {
            resources,
            compositions: Vec::new(),
        },
        inputs,
        &SourcePaths::new(Path::new(".")).unwrap(),
        Disclosure::Raw,
    )
    .unwrap_err();
    match error {
        ProbeError::Incomplete(message) => {
            assert!(message.starts_with("unattributed hash inputs"), "{message}");
            let (_, dump) = message
                .split_once('\n')
                .expect("incomplete error must carry its dump");
            let dump: Json = serde_json::from_str(dump).unwrap();
            assert_eq!(dump["unattributed"].as_array().unwrap().len(), 1);
            assert_eq!(dump["unattributed"][0]["input"]["writes"][0]["hex"], "07");
        }
        error => panic!("wrong probe error: {error:?}"),
    }
}

#[test]
fn observed_simhash_result_cannot_hide_missing_feature_inputs() {
    let resources = resources();
    let mut aggregate = input(&resources[0], 0);
    aggregate.boundary = "simhash";
    aggregate.writes.clear();
    aggregate.feature_count = Some(1);
    let inputs = vec![aggregate, input(&resources[1], 2)];
    let error = finish_dump(
        PreparedProbe {
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
    let dump = finish_dump(
        PreparedProbe {
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
    let error = finish_dump(
        PreparedProbe {
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
            message.starts_with("missing module prefix hash inputs for first"),
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
    ] {
        std::fs::create_dir_all(root.join(relative)).unwrap();
    }
    let output = temporary.path().join("dumps");
    let existing = output.join("aws/management/good/identity.json");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, "old dump").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let error = write_directory_dumps(&root, &output, |directory| {
        if directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("bad")
        {
            Err(ProbeError::Incomplete("fixture failure".to_owned()))
        } else {
            Ok("{\"redaction\":\"sha256\"}\n".to_owned())
        }
    })
    .unwrap_err();
    match &error {
        ProbeError::DirectoryFailures(failures) => assert_eq!(failures.failures().len(), 2),
        error => panic!("wrong batch error: {error:?}"),
    }
    let message = error.to_string();
    for directory in ["bad-first", "bad-last"] {
        assert!(message.contains(directory), "{message}");
    }
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
}
