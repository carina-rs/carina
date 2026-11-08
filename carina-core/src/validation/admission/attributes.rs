//! Catalog-bound resource and data-source attribute descriptors.

use super::*;
use crate::resource::ResourceId;
use crate::schema::{LeafPathSegment, SchemaKind, SchemaRegistry};

fn owner(catalog: &ProviderContext, provider: &str) -> DeclarationOwner {
    let mut owners = std::collections::BTreeSet::new();
    owners.extend(
        catalog
            .resource_types
            .iter()
            .map(|(owner, _)| owner.as_str()),
    );
    owners.extend(
        catalog
            .known_custom_types()
            .iter()
            .flat_map(|identity| identity.provider_names()),
    );
    owners
        .iter()
        .position(|owner| *owner == provider)
        .map(DeclarationOwner::Catalog)
        .unwrap_or(DeclarationOwner::Builtin)
}

impl SchemaRegistry {
    fn admission_destination<K: DestinationKind>(
        &self,
        id: &ResourceId,
        kind: SchemaKind,
        catalog: &ProviderContext,
    ) -> Destination<K> {
        let schema = self
            .get(&id.provider, &id.resource_type, kind)
            .map(|schema| schema.schema().admission_identity)
            .unwrap_or(self.unavailable_admission_identity);
        Destination::catalogued(
            DestinationIdentity {
                declaration: DeclarationIdentity {
                    owner: owner(catalog, &id.provider),
                    schema,
                },
                instance: InstanceIdentity::Resource(id.clone()),
                path: vec![],
                revision: CatalogRevision(catalog.custom_types.clone()),
            },
            format!("{id}"),
        )
    }

    pub fn resource_destination(
        &self,
        id: &ResourceId,
        catalog: &ProviderContext,
    ) -> Destination<kind::ResourceAttribute> {
        self.admission_destination(id, SchemaKind::Resource, catalog)
    }

    pub fn data_source_destination(
        &self,
        id: &ResourceId,
        catalog: &ProviderContext,
    ) -> Destination<kind::DataSourceAttribute> {
        self.admission_destination(id, SchemaKind::DataSource, catalog)
    }

    fn admission_attribute<K: DestinationKind>(
        &self,
        id: &ResourceId,
        attribute: &str,
        kind: SchemaKind,
        catalog: &ProviderContext,
    ) -> Option<Destination<K>> {
        let schema = self.get(&id.provider, &id.resource_type, kind)?;
        let effective = schema.effective_attribute_by_authored_name(attribute)?;
        let ordinal = schema
            .schema()
            .as_schema()
            .attributes()
            .keys()
            .position(|name| name == effective.name())?;
        let mut destination = self.admission_destination(id, kind, catalog);
        destination.identity.path.push(ordinal);
        destination.label = attribute.into();
        destination.contract = Some(Box::new(DestinationContract {
            attr: effective.field_type().clone(),
            defs: schema.schema().as_schema().defs.clone(),
            owner: schema.schema_type_owner().clone(),
            ancestry: FieldAncestry::from_input_mode(effective.effective_input_mode()),
            path: LeafPath::from_segments([LeafPathSegment::StructField(attribute.into())]),
        }));
        Some(destination)
    }

    pub fn resource_attribute_destination(
        &self,
        id: &ResourceId,
        attribute: &str,
        catalog: &ProviderContext,
    ) -> Option<Destination<kind::ResourceAttribute>> {
        self.admission_attribute(id, attribute, SchemaKind::Resource, catalog)
    }

    pub fn data_source_attribute_destination(
        &self,
        id: &ResourceId,
        attribute: &str,
        catalog: &ProviderContext,
    ) -> Option<Destination<kind::DataSourceAttribute>> {
        self.admission_attribute(id, attribute, SchemaKind::DataSource, catalog)
    }
}
