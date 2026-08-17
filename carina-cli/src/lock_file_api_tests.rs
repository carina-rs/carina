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
