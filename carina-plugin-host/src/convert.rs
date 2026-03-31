//! Conversions between carina-core types and carina-provider-protocol types.

use std::collections::HashMap;

use carina_core::resource::{
    LifecycleConfig as CoreLifecycle, Resource as CoreResource, ResourceId as CoreResourceId,
    State as CoreState, Value as CoreValue,
};
use carina_provider_protocol::types::{
    LifecycleConfig as ProtoLifecycle, Resource as ProtoResource, ResourceId as ProtoResourceId,
    State as ProtoState, Value as ProtoValue,
};

// -- ResourceId --

pub fn core_to_proto_resource_id(id: &CoreResourceId) -> ProtoResourceId {
    ProtoResourceId {
        provider: id.provider.clone(),
        resource_type: id.resource_type.clone(),
        name: id.name.clone(),
    }
}

pub fn proto_to_core_resource_id(id: &ProtoResourceId) -> CoreResourceId {
    CoreResourceId::with_provider(&id.provider, &id.resource_type, &id.name)
}

// -- Value --

pub fn core_to_proto_value(v: &CoreValue) -> ProtoValue {
    match v {
        CoreValue::String(s) => ProtoValue::String(s.clone()),
        CoreValue::Int(i) => ProtoValue::Int(*i),
        CoreValue::Float(f) => ProtoValue::Float(*f),
        CoreValue::Bool(b) => ProtoValue::Bool(*b),
        CoreValue::List(l) => ProtoValue::List(l.iter().map(core_to_proto_value).collect()),
        CoreValue::Map(m) => ProtoValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), core_to_proto_value(v)))
                .collect(),
        ),
        // ResourceRef, Interpolation, FunctionCall, Closure, Secret
        // should be resolved before reaching the provider.
        _ => ProtoValue::String(format!("{v:?}")),
    }
}

pub fn proto_to_core_value(v: &ProtoValue) -> CoreValue {
    match v {
        ProtoValue::String(s) => CoreValue::String(s.clone()),
        ProtoValue::Int(i) => CoreValue::Int(*i),
        ProtoValue::Float(f) => CoreValue::Float(*f),
        ProtoValue::Bool(b) => CoreValue::Bool(*b),
        ProtoValue::List(l) => CoreValue::List(l.iter().map(proto_to_core_value).collect()),
        ProtoValue::Map(m) => CoreValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), proto_to_core_value(v)))
                .collect(),
        ),
    }
}

pub fn core_to_proto_value_map(m: &HashMap<String, CoreValue>) -> HashMap<String, ProtoValue> {
    m.iter()
        .map(|(k, v)| (k.clone(), core_to_proto_value(v)))
        .collect()
}

pub fn proto_to_core_value_map(m: &HashMap<String, ProtoValue>) -> HashMap<String, CoreValue> {
    m.iter()
        .map(|(k, v)| (k.clone(), proto_to_core_value(v)))
        .collect()
}

// -- State --

pub fn core_to_proto_state(s: &CoreState) -> ProtoState {
    ProtoState {
        id: core_to_proto_resource_id(&s.id),
        identifier: s.identifier.clone(),
        attributes: core_to_proto_value_map(&s.attributes),
        exists: s.exists,
    }
}

pub fn proto_to_core_state(s: &ProtoState) -> CoreState {
    let id = proto_to_core_resource_id(&s.id);
    if s.exists {
        let mut state = CoreState::existing(id, proto_to_core_value_map(&s.attributes));
        if let Some(ref ident) = s.identifier {
            state = state.with_identifier(ident);
        }
        state
    } else {
        CoreState::not_found(id)
    }
}

// -- Resource --

pub fn core_to_proto_resource(r: &CoreResource) -> ProtoResource {
    ProtoResource {
        id: core_to_proto_resource_id(&r.id),
        attributes: core_to_proto_value_map(&r.resolved_attributes()),
        lifecycle: core_to_proto_lifecycle(&r.lifecycle),
    }
}

// -- LifecycleConfig --

pub fn core_to_proto_lifecycle(l: &CoreLifecycle) -> ProtoLifecycle {
    ProtoLifecycle {
        force_delete: l.force_delete,
        create_before_destroy: l.create_before_destroy,
    }
}
