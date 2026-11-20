use std::marker::PhantomData;

use carina_provider_resolver::LockFile;

/// Stable negative trait detection without specialization: the blanket trait
/// constant is the fallback, while the identically named inherent constant is
/// available (and takes precedence) only when `T: Serialize`.
trait SerializeStatus {
    const IS_SERIALIZE: bool = false;
}

impl<T> SerializeStatus for T {}

struct SerializeCheck<T>(PhantomData<T>);

impl<T: serde::Serialize> SerializeCheck<T> {
    const IS_SERIALIZE: bool = true;
}

#[derive(serde::Serialize)]
struct SerializableProbe;

struct NonSerializableProbe;

#[test]
fn lock_file_is_not_serializable_outside_provider_resolver() {
    let serializable_probe = SerializeCheck::<SerializableProbe>::IS_SERIALIZE;
    let non_serializable_probe = SerializeCheck::<NonSerializableProbe>::IS_SERIALIZE;
    let lock_file = SerializeCheck::<LockFile>::IS_SERIALIZE;

    // Keep both calibration assertions: they prove the detector selects each
    // branch, so this test cannot pass merely because it always returns false.
    assert!(serializable_probe);
    assert!(!non_serializable_probe);
    assert!(!lock_file);
}

trait DeserializeStatus {
    const IS_DESERIALIZE: bool = false;
}
impl<T> DeserializeStatus for T {}
struct DeserializeCheck<T>(PhantomData<T>);
impl<T: for<'de> serde::Deserialize<'de>> DeserializeCheck<T> {
    const IS_DESERIALIZE: bool = true;
}

#[test]
fn lock_entries_are_not_deserializable_outside_current_decoder() {
    let positive_control = DeserializeCheck::<String>::IS_DESERIALIZE;
    let negative_control = DeserializeCheck::<NonSerializableProbe>::IS_DESERIALIZE;
    let entry = DeserializeCheck::<carina_provider_resolver::LockEntry>::IS_DESERIALIZE;
    let kind = DeserializeCheck::<carina_provider_resolver::LockEntryKind>::IS_DESERIALIZE;
    let marker = DeserializeCheck::<carina_provider_resolver::NoRegistryLock>::IS_DESERIALIZE;
    assert!(positive_control);
    assert!(!negative_control);
    assert!(!entry);
    assert!(!kind);
    assert!(!marker);
}
