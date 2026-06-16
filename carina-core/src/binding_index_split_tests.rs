//! Tests for #3176: typed `ResolvedBindings` constructors.
//!
//! - `ResolvedBindings::pre_apply(PreApplyInputs)` is the unified pre-apply constructor;
//!   pre-apply bindings from a managed slice plus current state.
//! - `ResolvedBindings::layer_compositions_post_apply(&mut self, &[Composition])` —
//!   layer composition bindings on top, called only after managed bindings exist.
//!
//! the legacy `from_*_with_state` entries have been removed (carina#3248).
//! a thin shim over the two new entry points.

use std::collections::{BTreeSet, HashMap};

use indexmap::IndexMap;

use crate::binding_index::ResolvedBindings;
use crate::resource::{Composition, ConcreteValue, Resource, ResourceId, Signature, State, Value};

fn s(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}

fn make_managed(binding: &str, attrs: &[(&str, Value)]) -> Resource {
    let mut attributes = IndexMap::new();
    for (k, v) in attrs {
        attributes.insert((*k).into(), v.clone());
    }
    Resource {
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

fn make_virtual(binding: &str, attrs: &[(&str, Value)]) -> Composition {
    let mut attributes: IndexMap<String, crate::resource::CompositionAttribute> = IndexMap::new();
    for (k, v) in attrs {
        attributes.insert(
            (*k).into(),
            crate::resource::CompositionAttribute::from_value(v.clone()),
        );
    }
    Composition {
        id: ResourceId::new("_virtual.module", binding),
        signature: Signature {
            arguments: IndexMap::new(),
            attributes,
        },
        binding: Some(binding.into()),
        dependency_bindings: BTreeSet::new(),
        module_name: "m".into(),
        instance: binding.into(),
        quoted_string_attrs: Default::default(),
    }
}

#[test]
fn pre_apply_records_dsl_attributes() {
    let m = make_managed("a", &[("value", s("hello"))]);
    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[m],
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let view = bindings.get("a").expect("binding present");
    assert_eq!(view.get("value"), Some(&s("hello")));
}

#[test]
fn pre_apply_merges_state_for_missing_dsl_keys() {
    // DSL has `dsl_only`; state has `state_only` and the same `dsl_only`.
    // DSL wins on collision (pre-apply: trust the DSL).
    let m = make_managed("a", &[("dsl_only", s("from_dsl"))]);

    let mut state_attrs: HashMap<String, Value> = HashMap::new();
    state_attrs.insert("dsl_only".into(), s("from_state_should_be_ignored"));
    state_attrs.insert("state_only".into(), s("from_state"));
    let mut current_states = HashMap::new();
    current_states.insert(m.id.clone(), State::existing(m.id.clone(), state_attrs));

    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[m],
        compositions: &[],
        data_sources: &[],
        current_states: &crate::resource::into_plan_input_map(current_states.clone()),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let view = bindings.get("a").expect("binding present");
    assert_eq!(view.get("dsl_only"), Some(&s("from_dsl")));
    assert_eq!(view.get("state_only"), Some(&s("from_state")));
}

#[test]
fn pre_apply_skips_resources_without_binding() {
    let mut m = make_managed("a", &[("k", s("v"))]);
    m.binding = None;
    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[m],
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    assert!(bindings.get("a").is_none());
}

#[test]
fn add_compositions_layers_on_top_of_resources() {
    let managed = make_managed("a", &[("value", s("from_managed"))]);
    let mut bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[managed],
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    assert!(bindings.get("a").is_some());

    let virt = make_virtual("v", &[("role_arn", s("post_apply_arn"))]);
    bindings
        .layer_compositions_post_apply(&[virt])
        .expect("layer_compositions_post_apply");

    // Both bindings present after layering.
    assert!(bindings.get("a").is_some());
    let v_view = bindings.get("v").expect("composition binding present");
    assert_eq!(v_view.get("role_arn"), Some(&s("post_apply_arn")));
}

#[test]
fn add_compositions_overwrites_same_name_resource_binding() {
    // If a composition happens to share a binding name with a managed one,
    // layer_compositions_post_apply lands later → the composition wins. This is the
    // "post-apply view layers on the pre-apply view" semantic.
    let managed = make_managed("rd", &[("v", s("pre_apply"))]);
    let mut bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[managed],
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let virt = make_virtual("rd", &[("v", s("post_apply"))]);
    bindings
        .layer_compositions_post_apply(&[virt])
        .expect("layer_compositions_post_apply");

    let view = bindings.get("rd").expect("rd binding present");
    assert_eq!(view.get("v"), Some(&s("post_apply")));
}

#[test]
fn add_compositions_skips_unbound() {
    let mut virt = make_virtual("v", &[("k", s("x"))]);
    virt.binding = None;
    let mut bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[],
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    bindings
        .layer_compositions_post_apply(&[virt])
        .expect("layer_compositions_post_apply");
    assert!(bindings.get("v").is_none());
}

#[test]
fn managed_plus_virtual_layering() {
    // carina#3181: managed resources and composition resources are
    // separate typestates. `pre_apply` builds the pre-apply view
    // from all kinds (managed + composition + data_sources). This test
    // exercises the *increment* path used by `state_writeback`:
    // build via `pre_apply` with `compositions: &[]`, then layer them
    // on top via `layer_compositions_post_apply` after re-resolution.
    let managed = make_managed("a", &[("value", s("hello"))]);
    let virt = make_virtual("v", &[("forwarded", s("via_virtual"))]);

    let mut typed = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: std::slice::from_ref(&managed),
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    typed
        .layer_compositions_post_apply(std::slice::from_ref(&virt))
        .expect("layer_compositions_post_apply");

    assert_eq!(
        typed.get("a").and_then(|m| m.get("value")),
        Some(&s("hello")),
        "managed binding mismatch",
    );
    assert_eq!(
        typed.get("v").and_then(|m| m.get("forwarded")),
        Some(&s("via_virtual")),
        "composition binding mismatch",
    );
}
