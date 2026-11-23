//! Writeback fixtures pass desired resources through real provider preparation.

use crate::executor::{ModuleConstraintGate, ProviderPreparationContext, ProviderPreparationError};
use crate::provider::{AppliedIdentifier, NoopNormalizer};
use crate::replacement::{CreatePermit, UpdatePermit};
use crate::resource::{AbsoluteBindingAddress, NoPlaceholder, Resource, SnapshotState};
use crate::schema::{SchemaKind, SchemaRegistry};

fn desired_resource<S, D>(
    desired: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
) -> Resource {
    let mut resource = Resource::from_id(desired.id.clone());
    resource.attributes = desired.attributes.clone();
    resource.directives = desired.directives.clone();
    resource.prefixes = desired.prefixes.clone();
    resource
}

fn with_preparation<T>(
    schemas: &SchemaRegistry,
    prepare: impl FnOnce(&ProviderPreparationContext<'_>) -> T,
) -> T {
    let bindings = crate::binding_index::ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let loaded = super::loaded_fixture_snapshot(schemas.clone());
    crate::validation::deferred_types::with_session_for_test(
        loaded.provider_context(),
        |type_checks| {
            prepare(&ProviderPreparationContext::for_test(
                &bindings,
                &gate,
                &[],
                &NoopNormalizer,
                schemas,
                type_checks,
            ))
        },
    )
}

pub fn create_permit<S, D>(
    desired: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
) -> CreatePermit {
    try_create_permit(desired).expect("fixture create passes provider preparation")
}

pub fn try_create_permit<S, D>(
    desired: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
) -> Result<CreatePermit, ProviderPreparationError> {
    let desired = desired_resource(desired);
    let schemas = super::preparation_schemas_with_attributes(
        &desired.id,
        SchemaKind::Resource,
        &desired.attributes,
    );
    prepare_create(desired, &schemas)
}

/// Use the fixture's declarations when inputs include DSL enums or constraints.
pub fn create_permit_with_schemas<S, D>(
    desired: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    schemas: &SchemaRegistry,
) -> CreatePermit {
    prepare_create(desired_resource(desired), schemas)
        .expect("fixture create passes provider preparation")
}

fn prepare_create(
    desired: Resource,
    schemas: &SchemaRegistry,
) -> Result<CreatePermit, ProviderPreparationError> {
    with_preparation(schemas, |context| {
        let request = super::provider_text::block_on(crate::executor::prepare_create_request(
            desired, context,
        ))?;
        Ok(crate::replacement::create_permit_with_schemas_for_test(
            request, schemas,
        ))
    })
}

pub fn update_permit<S, D>(
    desired: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    from: SnapshotState,
    changed: &[&str],
) -> UpdatePermit {
    try_update_permit(desired, from, changed).expect("fixture update passes provider preparation")
}

/// Preserve declared map/struct paths or provider constraints in update fixtures.
pub fn update_permit_with_schemas<S, D>(
    desired: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    from: SnapshotState,
    changed: &[&str],
    schemas: &SchemaRegistry,
) -> UpdatePermit {
    prepare_update(desired_resource(desired), from, changed, schemas)
        .expect("fixture update passes provider preparation")
}

pub fn try_update_permit<S, D>(
    desired: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    from: SnapshotState,
    changed: &[&str],
) -> Result<UpdatePermit, ProviderPreparationError> {
    let desired = desired_resource(desired);
    let mut schemas = super::preparation_schemas_with_attributes(
        &desired.id,
        SchemaKind::Resource,
        &desired.attributes,
    );
    super::complete_fixture_schema(
        &mut schemas,
        &desired.id,
        SchemaKind::Resource,
        &from.attributes.clone().into_iter().collect(),
    );
    prepare_update(desired, from, changed, &schemas)
}

fn prepare_update(
    desired: Resource,
    from: SnapshotState,
    changed: &[&str],
    schemas: &SchemaRegistry,
) -> Result<UpdatePermit, ProviderPreparationError> {
    with_preparation(schemas, |context| {
        let request = super::provider_text::block_on(crate::executor::prepare_update_request(
            desired,
            from,
            &changed
                .iter()
                .map(|key| (*key).to_owned())
                .collect::<Vec<_>>(),
            context,
        ))?;
        Ok(crate::replacement::update_permit_with_schemas_for_test(
            AppliedIdentifier::new("fixture-id").unwrap(),
            request,
            schemas,
        ))
    })
}
