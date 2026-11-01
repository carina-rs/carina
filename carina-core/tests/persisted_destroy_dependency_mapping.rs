use carina_core::resource::{
    NotAManagedRow, PersistedDestroyDependencies, ResolvedResourceId, ResourceId,
};
use std::collections::BTreeSet;

fn managed() -> PersistedDestroyDependencies {
    PersistedDestroyDependencies::decode_persisted(BTreeSet::from([ResolvedResourceId::new(
        ResourceId::with_provider_identity("mock", "test.resource", "source", None),
    )]))
    .unwrap()
}

#[test]
fn mapped_destroy_dependencies_reject_wait_ids() {
    let result = managed().try_map_addresses(|_| -> Result<_, NotAManagedRow> {
        Ok(ResolvedResourceId::with_identity("__wait", "ready"))
    });
    assert!(
        result.is_err(),
        "address mapping must validate the managed-row invariant"
    );
}

#[test]
fn mapped_destroy_dependencies_reject_providerless_ids() {
    let result = managed().try_map_addresses(|_| -> Result<_, NotAManagedRow> {
        Ok(ResolvedResourceId::new(ResourceId::with_identity(
            "test.resource",
            "unrouted",
        )))
    });
    assert!(
        result.is_err(),
        "address mapping must reject provider-less row edges"
    );
}

#[test]
fn mapped_destroy_dependencies_preserve_valid_managed_rows() {
    let mapped = managed()
        .try_map_addresses(|_| -> Result<_, NotAManagedRow> {
            Ok(ResolvedResourceId::new(ResourceId::with_provider_identity(
                "mock",
                "test.resource",
                "canonical",
                None,
            )))
        })
        .unwrap();
    assert_eq!(mapped.iter().next().unwrap().identity_str(), "canonical");
}
