use super::*;
use indexmap::IndexMap;
use std::hash::{DefaultHasher, Hash};

use crate::identifier::compute_anonymous_identifiers_for_test;
use crate::resource::{ConcreteValue, DeferredValue, Resource, SecretPayload, Value};
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

#[derive(Default)]
struct Calls(Vec<HashWrite>);

impl Hasher for Calls {
    fn finish(&self) -> u64 {
        self.0.len() as u64
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.push(HashWrite {
            operation: "write",
            bytes: bytes.to_vec(),
        });
    }
    fn write_u8(&mut self, value: u8) {
        self.0.push(HashWrite {
            operation: "write_u8",
            bytes: value.to_le_bytes().to_vec(),
        });
    }
    fn write_u64(&mut self, value: u64) {
        self.0.push(HashWrite {
            operation: "write_u64",
            bytes: value.to_le_bytes().to_vec(),
        });
    }
}

fn hash_inputs(hasher: &mut impl Hasher) {
    for length in [0, 1, 7, 8, 9, 15, 16, 17, 63, 64, 65, 129] {
        "x".repeat(length).hash(hasher);
    }
    hasher.write(&[]);
    hasher.write(b"a");
    hasher.write(b"bc");
    hasher.write_u64(0x0123_4567_89ab_cdef);
    hasher.write_u8(0xff);
}

#[test]
fn forwarding_preserves_every_write_boundary_and_default_hasher_result() {
    let mut calls = Calls::default();
    hash_inputs(&mut calls);
    let (actual_calls, inputs) = record(|| {
        let mut wrapped = RecordingHasher::new(Calls::default(), "test");
        hash_inputs(&mut wrapped);
        wrapped.inner.0.clone()
    });
    assert_eq!(calls.0, actual_calls);
    assert_eq!(inputs[0].writes, calls.0);

    let mut plain = DefaultHasher::new();
    hash_inputs(&mut plain);
    let mut idle = RecordingHasher::new(DefaultHasher::new(), "test");
    hash_inputs(&mut idle);
    let (recorded, _) = record(|| {
        let mut wrapped = RecordingHasher::new(DefaultHasher::new(), "test");
        hash_inputs(&mut wrapped);
        wrapped.finish()
    });
    assert_eq!(plain.finish(), idle.finish());
    assert_eq!(plain.finish(), recorded);
}

#[test]
fn sha256_forwarding_preserves_updates_and_digest() {
    let chunks: &[&[u8]] = &[b"", b"a", b"bc", &[0; 65], b"last"];
    let mut plain = Sha256::new();
    for chunk in chunks {
        plain.update(chunk);
    }
    let (digest, inputs) = record(|| {
        let mut wrapped = RecordingSha256::new(Sha256::new(), "test");
        for chunk in chunks {
            wrapped.update(chunk);
        }
        wrapped.finalize()
    });
    assert_eq!(plain.finalize(), digest);
    assert_eq!(
        inputs[0]
            .writes
            .iter()
            .map(|write| write.bytes.as_slice())
            .collect::<Vec<_>>(),
        chunks
    );
    assert_eq!(inputs[0].output.as_deref(), Some(digest.as_slice()));
}

#[test]
fn identity_addresses_and_secret_hashes_are_identical_with_an_idle_or_active_recorder() {
    let mut registry = SchemaRegistry::new();
    for (name, attribute) in [
        (
            "Declared",
            AttributeSchema::optional("name", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        ),
        (
            "CreateOnly",
            AttributeSchema::optional("name", AttributeType::string())
                .create_only()
                .build(),
        ),
        (
            "Mutable",
            AttributeSchema::optional("name", AttributeType::string()).build(),
        ),
    ] {
        registry.insert(
            "probe",
            ResourceSchema::new(name)
                .attribute(attribute)
                .validate()
                .unwrap(),
        );
    }
    let resources: Vec<Resource> = ["Declared", "CreateOnly", "Mutable"]
        .into_iter()
        .map(|ty| {
            let mut resource = Resource::pending_with_provider("probe", ty, None);
            resource.set_attr(
                "name",
                Value::Concrete(ConcreteValue::String("unchanged".into())),
            );
            resource
        })
        .collect();
    let compute = || {
        let mut resources = resources.clone();
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .unwrap();
        resources
            .into_iter()
            .map(|resource| (resource.id.to_string(), resource.id))
            .collect::<Vec<_>>()
    };
    let idle = compute();
    let (active, inputs) = record(compute);
    assert_eq!(idle, active);
    assert_eq!(idle, compute());
    for boundary in ["declared-identity", "create-only", "simhash-feature"] {
        assert!(inputs.iter().any(|input| input.boundary == boundary));
    }

    let secret: SecretPayload<_, _> =
        Box::new(Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "literal".to_owned(),
                Value::Concrete(ConcreteValue::String("secret".into())),
            ),
            (
                "list".to_owned(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Int(7),
                )])),
            ),
        ]))))
        .into();
    let baseline = secret.hash(None).unwrap();
    let idle_hash = secret.hash(Some(&baseline)).unwrap();
    let (active_hash, material) = record(|| secret.hash(Some(&baseline)).unwrap());
    assert_eq!(idle_hash, active_hash);
    assert_eq!(baseline, active_hash);
    assert_eq!(material.len(), 1);
    assert_eq!(material[0].boundary, "secret-material");
    assert!(secret.verify_hash(&active_hash).unwrap());
    let _typed_value: Value = Value::Deferred(DeferredValue::Secret(secret));
}

#[test]
fn nested_recorders_and_unwinding_restore_the_previous_scope() {
    let (_, outer) = record(|| {
        let _context = secret_producer_context(SecretProducer::PayloadHash);
        secret_material(b"before");
        let (_, inner) = record(|| secret_material(b"inner"));
        assert_eq!(inner.len(), 1);
        assert!(inner[0].context.is_empty());
        let panic = std::panic::catch_unwind(|| record(|| panic!("test unwind")));
        assert!(panic.is_err());
        secret_material(b"after");
    });
    assert_eq!(outer.len(), 2);
    assert!(
        outer
            .iter()
            .all(|input| input.context
                == [InputContext::SecretProducer(SecretProducer::PayloadHash)])
    );
    assert!(ACTIVE.with(|active| active.borrow().is_none()));
}

#[test]
fn eight_in_process_threads_do_not_share_recordings() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0_u8..8)
        .map(|index| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let (_, inputs) = record(|| {
                    let _context = secret_producer_context(SecretProducer::PayloadHash);
                    barrier.wait();
                    secret_material(&[index]);
                });
                assert_eq!(inputs.len(), 1);
                assert_eq!(
                    inputs[0].context,
                    [InputContext::SecretProducer(SecretProducer::PayloadHash)]
                );
                assert_eq!(inputs[0].writes[0].bytes, [index]);
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn str_and_string_hashes_and_finish_outputs_match_plain_default_hasher() {
    for text in [
        "",
        "x",
        "abcdefgh",
        "a string longer than one word",
        "日本語\0",
    ] {
        let mut plain = DefaultHasher::new();
        text.hash(&mut plain);
        let expected = plain.finish();
        let (actual, inputs) = record(|| {
            let mut hasher = RecordingHasher::new(DefaultHasher::new(), "str");
            text.hash(&mut hasher);
            let first = hasher.finish();
            assert_eq!(first, hasher.finish());
            first
        });
        assert_eq!(actual, expected);
        assert_eq!(inputs[0].output, Some(expected.to_le_bytes().to_vec()));
        assert_eq!(inputs[0].finishes.len(), 2);

        let (actual, inputs) = record(|| {
            let mut hasher = RecordingHasher::new(DefaultHasher::new(), "String");
            text.to_owned().hash(&mut hasher);
            hasher.finish()
        });
        assert_eq!(actual, expected);
        assert_eq!(inputs[0].output, Some(expected.to_le_bytes().to_vec()));
    }
}

#[test]
fn integer_observations_have_fixed_little_endian_encoding() {
    let (_, inputs) = record(|| {
        let mut hasher = RecordingHasher::new(DefaultHasher::new(), "integers");
        hasher.write_u32(0x0123_4567);
        hasher.write_i16(-2);
        hasher.write_usize(7);
        hasher.write_isize(-3);
        let _ = hasher.finish();
    });
    let bytes: Vec<_> = inputs[0]
        .writes
        .iter()
        .map(|write| write.bytes.clone())
        .collect();
    assert_eq!(
        bytes,
        [
            vec![0x67, 0x45, 0x23, 0x01],
            vec![0xfe, 0xff],
            7_u64.to_le_bytes().to_vec(),
            (-3_i64).to_le_bytes().to_vec()
        ]
    );
}

#[test]
fn identity_inputs_follow_parser_ids_across_reordered_and_separate_slices() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "probe",
        ResourceSchema::new("Declared")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file(
        "probe.Declared { name = 'first' }\nprobe.Declared { name = 'second' }\n",
        std::path::Path::new("system-ids.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let resources: Vec<Resource> = parsed
        .resources
        .into_iter()
        .map(|resource| {
            resource
                .resolve_root_for_test()
                .into_expanded(&registry)
                .unwrap()
        })
        .collect();
    let keys: Vec<_> = resources
        .iter()
        .map(|resource| resource_key(resource).unwrap())
        .collect();
    assert_ne!(keys[0], keys[1]);
    let (_, inputs) = record(|| {
        for index in [1, 0] {
            let mut slice = vec![resources[index].clone()];
            compute_anonymous_identifiers_for_test(&mut slice, &[], &registry, &|_| Vec::new())
                .unwrap();
            assert_eq!(resource_key(&slice[0]).unwrap(), keys[index]);
        }
    });
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].owner, InputOwner::Resource(keys[1].clone()));
    assert_eq!(inputs[1].owner, InputOwner::Resource(keys[0].clone()));
}

#[test]
fn loop_result_association_comes_from_the_identity_producer() {
    let (parsed, inputs) = record(|| {
        crate::parser::parse_file(
            "for name in ['first', 'second'] { probe.Declared { name = name } }\n",
            std::path::Path::new("loop-link.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap()
    });
    let identities = loop_system_identities(&parsed.resources[0].loop_provenance);
    assert!(!identities.is_empty());
    for identity in identities {
        assert!(inputs.iter().any(|input| matches!(&input.owner, InputOwner::Loop { identity: recorded, .. } if recorded == &identity)));
    }
}

#[test]
fn off_thread_identity_computation_cannot_silently_enter_the_callers_recording() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "probe",
        ResourceSchema::new("Declared")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let mut resource: Resource = Resource::pending_with_provider("probe", "Declared", None);
    resource.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("worker".into())),
    );
    let (resources, inputs) = record(|| {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let mut resources = vec![resource];
                    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| {
                        Vec::new()
                    })
                    .unwrap();
                    resources
                })
                .join()
                .unwrap()
        })
    });
    assert!(resources[0].id.identity().is_some());
    // The facade must reject these missing observations, rather than returning
    // a successful empty hash_inputs array (covered by its missing-input test).
    assert!(inputs.is_empty());
}

#[test]
fn fingerprints_pin_sha256_and_byte_length() {
    let fingerprint = fingerprint(b"abc");
    assert_eq!(fingerprint.length, 3);
    assert_eq!(
        fingerprint.sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn probe_secret_observations_use_production_payload_and_snapshot_hashes() {
    let payload: SecretPayload<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    > = Box::new(Value::Concrete(ConcreteValue::String(
        "producer-canary".into(),
    )))
    .into();
    let (_, production) = record(|| payload.hash(None).unwrap());
    let (_, observed) = record(|| observe_secret(&payload).unwrap());
    assert_eq!(observed, production);
    assert_eq!(
        observed[0].owner,
        InputOwner::SecretProducer(SecretProducer::PayloadHash)
    );
    let (_, snapshot) = record(|| observe_snapshot_secret(&payload).unwrap());
    assert_eq!(snapshot.len(), 1);
    assert_eq!(
        snapshot[0].owner,
        InputOwner::SecretProducer(SecretProducer::SnapshotHash)
    );
    assert_eq!(snapshot[0].writes, production[0].writes);
}

#[test]
fn module_source_recording_is_an_additive_provided_method() {
    struct ExternalModuleSource;
    impl crate::resource::AnonymousIdentityModuleSource for ExternalModuleSource {
        fn anonymous_outer_instance_identity(&self) -> Option<crate::resource::ResourceIdentity> {
            None
        }
        fn anonymous_scope(&self) -> Option<(String, String)> {
            None
        }
        fn anonymous_authored_locations(
            &self,
        ) -> Vec<(std::path::PathBuf, crate::parser::SourceSpan)> {
            Vec::new()
        }
    }
    use crate::resource::AnonymousIdentityModuleSource;
    assert!(ExternalModuleSource.recording_module_calls().is_none());
}

#[test]
fn address_key_observations_preserve_the_production_address_and_source_binding_names() {
    let (_, _) = record(|| {
        let safe = crate::utils::map_key_address("named", "safe_private_key");
        let unsafe_key = crate::utils::map_key_address(&safe, "private-key.with space");
        assert_eq!(safe, "named.safe_private_key");
        assert_eq!(
            unsafe_key,
            "named.safe_private_key['private-key.with space']"
        );
        assert_eq!(
            address_parts(&format!("module.{unsafe_key}.child")),
            [
                AddressPart::Literal("module.named".into()),
                AddressPart::MapKey("safe_private_key".into()),
                AddressPart::MapKey("private-key.with space".into()),
                AddressPart::Literal(".child".into()),
            ]
        );
    });
}
