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
            bytes: value.to_ne_bytes().to_vec(),
        });
    }
    fn write_u64(&mut self, value: u64) {
        self.0.push(HashWrite {
            operation: "write_u64",
            bytes: value.to_ne_bytes().to_vec(),
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
        let _context = context(|| "outer".to_owned());
        secret_material(b"before");
        let (_, inner) = record(|| secret_material(b"inner"));
        assert_eq!(inner.len(), 1);
        assert!(inner[0].context.is_empty());
        let panic = std::panic::catch_unwind(|| record(|| panic!("test unwind")));
        assert!(panic.is_err());
        secret_material(b"after");
    });
    assert_eq!(outer.len(), 2);
    assert!(outer.iter().all(|input| input.context == ["outer"]));
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
                    let _context = context(|| format!("thread:{index}"));
                    barrier.wait();
                    secret_material(&[index]);
                });
                assert_eq!(inputs.len(), 1);
                assert_eq!(inputs[0].context, [format!("thread:{index}")]);
                assert_eq!(inputs[0].writes[0].bytes, [index]);
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}
