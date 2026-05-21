//! Tests for #3176: typed `ResolvedBindings` constructors.
//!
//! - `ResolvedBindings::from_managed_with_state(&[ManagedResource], …)` —
//!   pre-apply bindings from a managed slice plus current state.
//! - `ResolvedBindings::add_virtual_resources(&mut self, &[VirtualResource])` —
//!   layer virtual bindings on top, called only after managed bindings exist.
//!
//! The legacy `from_resources_with_state(&[Resource], …)` constructor is
//! a thin shim over the two new entry points.

use std::collections::{BTreeSet, HashMap};

use indexmap::IndexMap;

use crate::binding_index::ResolvedBindings;
use crate::resource::{
    ConcreteValue, ManagedResource, Resource, ResourceId, State, Value, VirtualResource,
};

fn s(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}

fn make_managed(binding: &str, attrs: &[(&str, Value)]) -> ManagedResource {
    let mut attributes = IndexMap::new();
    for (k, v) in attrs {
        attributes.insert((*k).into(), v.clone());
    }
    ManagedResource {
        id: ResourceId::new("aws.s3.Bucket", binding),
        attributes,
        directives: Default::default(),
        prefixes: HashMap::new(),
        binding: Some(binding.into()),
        dependency_bindings: BTreeSet::new(),
        module_source: None,
        quoted_string_attrs: Default::default(),
    }
}

fn make_virtual(binding: &str, attrs: &[(&str, Value)]) -> VirtualResource {
    let mut attributes = IndexMap::new();
    for (k, v) in attrs {
        attributes.insert((*k).into(), v.clone());
    }
    VirtualResource {
        id: ResourceId::new("_virtual.module", binding),
        attributes,
        binding: Some(binding.into()),
        dependency_bindings: BTreeSet::new(),
        module_name: "m".into(),
        instance: binding.into(),
        quoted_string_attrs: Default::default(),
    }
}

#[test]
fn from_managed_with_state_records_dsl_attributes() {
    let m = make_managed("a", &[("value", s("hello"))]);
    let bindings =
        ResolvedBindings::from_managed_with_state(&[m], &HashMap::new(), &HashMap::new(), &[]);
    let view = bindings.get("a").expect("binding present");
    assert_eq!(view.get("value"), Some(&s("hello")));
}

#[test]
fn from_managed_with_state_merges_state_for_missing_dsl_keys() {
    // DSL has `dsl_only`; state has `state_only` and the same `dsl_only`.
    // DSL wins on collision (pre-apply: trust the DSL).
    let m = make_managed("a", &[("dsl_only", s("from_dsl"))]);

    let mut state_attrs: HashMap<String, Value> = HashMap::new();
    state_attrs.insert("dsl_only".into(), s("from_state_should_be_ignored"));
    state_attrs.insert("state_only".into(), s("from_state"));
    let mut current_states = HashMap::new();
    current_states.insert(m.id.clone(), State::existing(m.id.clone(), state_attrs));

    let bindings =
        ResolvedBindings::from_managed_with_state(&[m], &current_states, &HashMap::new(), &[]);
    let view = bindings.get("a").expect("binding present");
    assert_eq!(view.get("dsl_only"), Some(&s("from_dsl")));
    assert_eq!(view.get("state_only"), Some(&s("from_state")));
}

#[test]
fn from_managed_with_state_skips_resources_without_binding() {
    let mut m = make_managed("a", &[("k", s("v"))]);
    m.binding = None;
    let bindings =
        ResolvedBindings::from_managed_with_state(&[m], &HashMap::new(), &HashMap::new(), &[]);
    assert!(bindings.get("a").is_none());
}

#[test]
fn add_virtual_resources_layers_on_top_of_managed() {
    let managed = make_managed("a", &[("value", s("from_managed"))]);
    let mut bindings = ResolvedBindings::from_managed_with_state(
        &[managed],
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );
    assert!(bindings.get("a").is_some());

    let virt = make_virtual("v", &[("role_arn", s("post_apply_arn"))]);
    bindings
        .add_virtual_resources(&[virt])
        .expect("add_virtual_resources");

    // Both bindings present after layering.
    assert!(bindings.get("a").is_some());
    let v_view = bindings.get("v").expect("virtual binding present");
    assert_eq!(v_view.get("role_arn"), Some(&s("post_apply_arn")));
}

#[test]
fn add_virtual_resources_overwrites_same_name_managed_binding() {
    // If a virtual happens to share a binding name with a managed one,
    // add_virtual_resources lands later → the virtual wins. This is the
    // "post-apply view layers on the pre-apply view" semantic.
    let managed = make_managed("rd", &[("v", s("pre_apply"))]);
    let mut bindings = ResolvedBindings::from_managed_with_state(
        &[managed],
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );
    let virt = make_virtual("rd", &[("v", s("post_apply"))]);
    bindings
        .add_virtual_resources(&[virt])
        .expect("add_virtual_resources");

    let view = bindings.get("rd").expect("rd binding present");
    assert_eq!(view.get("v"), Some(&s("post_apply")));
}

#[test]
fn add_virtual_resources_skips_unbound() {
    let mut virt = make_virtual("v", &[("k", s("x"))]);
    virt.binding = None;
    let mut bindings =
        ResolvedBindings::from_managed_with_state(&[], &HashMap::new(), &HashMap::new(), &[]);
    bindings
        .add_virtual_resources(&[virt])
        .expect("add_virtual_resources");
    assert!(bindings.get("v").is_none());
}

#[test]
fn legacy_constructor_equivalence_managed_plus_virtual() {
    // Feeding the same inputs through the legacy `from_resources_with_state`
    // (mixed Resource slice) and through the new managed+virtual pair
    // must yield identical bindings views — same binding names, same
    // attribute maps, same source kinds.
    let managed = make_managed("a", &[("value", s("hello"))]);
    let virt = make_virtual("v", &[("forwarded", s("via_virtual"))]);

    // New typed path:
    let mut typed = ResolvedBindings::from_managed_with_state(
        std::slice::from_ref(&managed),
        &HashMap::new(),
        &HashMap::new(),
        &[],
    );
    typed
        .add_virtual_resources(std::slice::from_ref(&virt))
        .expect("add_virtual_resources");

    // Legacy mixed-slice path:
    let mixed: Vec<Resource> = vec![Resource::from(&managed), virtual_as_resource(&virt)];
    let legacy =
        ResolvedBindings::from_resources_with_state(&mixed, &HashMap::new(), &HashMap::new(), &[]);

    assert_eq!(typed.get("a"), legacy.get("a"), "managed binding mismatch");
    assert_eq!(typed.get("v"), legacy.get("v"), "virtual binding mismatch");
}

/// Reverse of [`From<&ManagedResource> for Resource`]: rebuild a
/// `Resource` from a `VirtualResource` for the legacy-path equivalence
/// test. Local to the test — production code lives behind the typed
/// entry once #3177+ migrates the call sites.
fn virtual_as_resource(v: &VirtualResource) -> Resource {
    use crate::resource::ResourceKind;
    Resource {
        id: v.id.clone(),
        attributes: v.attributes.clone(),
        kind: ResourceKind::Virtual {
            module_name: v.module_name.clone(),
            instance: v.instance.clone(),
        },
        directives: Default::default(),
        prefixes: HashMap::new(),
        binding: v.binding.clone(),
        dependency_bindings: v.dependency_bindings.clone(),
        module_source: None,
        quoted_string_attrs: v.quoted_string_attrs.clone(),
    }
}
