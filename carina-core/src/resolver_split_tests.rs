//! Tests for #3175: typed resolver entry points.
//!
//! - `resolve_managed_refs_with_state_and_remote(&mut [Resource], ...)`
//!   — pre-apply path, accepts only `Resource` slices.
//! - `resolve_virtual_refs_post_apply(&mut [Composition], &ResolvedBindings)`
//!   — post-apply path, takes a pre-built bindings view.
//!
//! Both are new entry points layered over the existing
//! `resolve_refs_inner` logic; the legacy
//! `resolve_refs_with_state_and_remote(&mut [Resource], …)` shim is
//! unchanged.

use std::collections::{BTreeSet, HashMap};

use indexmap::IndexMap;

use crate::binding_index::ResolvedBindings;
use crate::resolver::{
    resolve_managed_refs_with_state_and_remote, resolve_virtual_refs_post_apply,
};
use crate::resource::{
    AccessPath, Composition, ConcreteValue, DeferredValue, Resource, ResourceId, Signature, State,
    Value,
};

fn s(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}

fn ref_to(binding: &str, attr: &str) -> Value {
    Value::Deferred(DeferredValue::ResourceRef {
        path: AccessPath::new(binding, attr),
    })
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
    let mut attributes = IndexMap::new();
    for (k, v) in attrs {
        attributes.insert((*k).into(), v.clone());
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
fn resolve_managed_refs_resolves_resource_ref_against_managed_sibling() {
    // Two managed resources, `b` references `a.value`.
    let a = make_managed("a", &[("value", s("hello"))]);
    let b = make_managed("b", &[("dep", ref_to("a", "value"))]);
    let mut managed = vec![a, b];

    {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &managed,
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        resolve_managed_refs_with_state_and_remote(&mut managed, &bindings)
    }
    .expect("resolve managed");

    // After resolution `b.dep` should be the literal string from `a`.
    let dep = managed[1].attributes.get("dep").expect("dep present");
    assert_eq!(*dep, s("hello"), "expected resolved string, got {dep:?}");
}

#[test]
fn resolve_managed_refs_records_dependency_bindings() {
    let a = make_managed("a", &[("value", s("hello"))]);
    let b = make_managed("b", &[("dep", ref_to("a", "value"))]);
    let mut managed = vec![a, b];

    {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &managed,
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        resolve_managed_refs_with_state_and_remote(&mut managed, &bindings)
    }
    .expect("resolve managed");

    assert!(
        managed[1].dependency_bindings.contains("a"),
        "expected dependency on `a`, got {:?}",
        managed[1].dependency_bindings,
    );
}

#[test]
fn resolve_managed_refs_falls_through_state_attributes() {
    // `b` references `a.value`, but `a` has no `value` attribute in
    // its DSL — the resolved binding must come from `current_states`.
    let a = make_managed("a", &[]);
    let b = make_managed("b", &[("dep", ref_to("a", "value"))]);
    let mut managed = vec![a, b];

    let mut state_attrs: HashMap<String, Value> = HashMap::new();
    state_attrs.insert("value".into(), s("from_state"));
    let mut current_states = HashMap::new();
    current_states.insert(
        managed[0].id.clone(),
        State::existing(managed[0].id.clone(), state_attrs),
    );

    {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &managed,
            compositions: &[],
            data_sources: &[],
            current_states: &current_states,
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        resolve_managed_refs_with_state_and_remote(&mut managed, &bindings)
    }
    .expect("resolve managed");

    let dep = managed[1].attributes.get("dep").expect("dep present");
    assert_eq!(*dep, s("from_state"), "expected state value, got {dep:?}");
}

#[test]
fn resolve_virtual_refs_post_apply_uses_provided_bindings() {
    // Bindings built externally (#3177's job at apply time). Verify
    // that the post-apply entry resolves `Composition` refs
    // against the supplied view.
    let referenced = make_managed("a", &[("value", s("post_apply_value"))]);
    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: std::slice::from_ref(&referenced),
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let mut compositions = vec![make_virtual("v", &[("forwarded", ref_to("a", "value"))])];
    resolve_virtual_refs_post_apply(&mut compositions, &bindings).expect("resolve compositions");

    let forwarded = compositions[0]
        .signature
        .attributes
        .get("forwarded")
        .expect("forwarded present");
    assert_eq!(*forwarded, s("post_apply_value"));
}

#[test]
fn resolve_virtual_refs_post_apply_picks_post_apply_value_not_pre_apply() {
    // #3169 root cause: a composition that references a managed Role
    // gets the *pre-apply* ARN if resolution runs against the
    // pre-apply state, and the *post-apply* ARN if it runs against
    // the post-apply state. Verify the post-apply entry selects the
    // post-apply value when handed the post-apply bindings view.
    let role = make_managed("role", &[("arn", s("post_apply_arn"))]);
    let post_apply_bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: std::slice::from_ref(&role),
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let mut compositions = vec![make_virtual("rd", &[("role_arn", ref_to("role", "arn"))])];
    resolve_virtual_refs_post_apply(&mut compositions, &post_apply_bindings)
        .expect("resolve compositions");

    let role_arn = compositions[0]
        .signature
        .attributes
        .get("role_arn")
        .expect("role_arn present");
    assert_eq!(
        *role_arn,
        s("post_apply_arn"),
        "expected post-apply ARN, got {role_arn:?}",
    );
}

#[test]
fn resolve_virtual_refs_post_apply_leaves_non_ref_values_intact() {
    // A literal attribute on a `Composition` should be preserved
    // verbatim by the post-apply pass.
    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[],
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let mut compositions = vec![make_virtual("v", &[("literal", s("kept"))])];

    resolve_virtual_refs_post_apply(&mut compositions, &bindings).expect("resolve compositions");

    let literal = compositions[0]
        .signature
        .attributes
        .get("literal")
        .expect("literal present");
    assert_eq!(*literal, s("kept"));
}

#[test]
fn resolve_managed_refs_with_empty_slice_is_ok() {
    let mut managed: Vec<Resource> = Vec::new();
    {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &managed,
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        resolve_managed_refs_with_state_and_remote(&mut managed, &bindings)
    }
    .expect("empty managed slice resolves cleanly");
    assert!(managed.is_empty());
}

#[test]
fn resolve_virtual_refs_post_apply_with_empty_slice_is_ok() {
    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &[],
        compositions: &[],
        data_sources: &[],
        current_states: &HashMap::new(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let mut compositions: Vec<Composition> = Vec::new();
    resolve_virtual_refs_post_apply(&mut compositions, &bindings)
        .expect("empty composition slice resolves cleanly");
    assert!(compositions.is_empty());
}

#[test]
fn resolve_managed_refs_legacy_shim_produces_identical_result() {
    // Equivalence guard: feeding the same managed-only inputs through
    // the new typed entry and the legacy `&mut [Resource]` shim must
    // produce identical attributes.
    let a_new = make_managed("a", &[("value", s("hi"))]);
    let b_new = make_managed("b", &[("dep", ref_to("a", "value"))]);
    let mut managed = vec![a_new.clone(), b_new.clone()];

    {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &managed,
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        resolve_managed_refs_with_state_and_remote(&mut managed, &bindings)
    }
    .expect("resolve managed");

    let mut legacy: Vec<Resource> = vec![a_new.clone(), b_new.clone()];
    {
        let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
            managed: &legacy,
            compositions: &[],
            data_sources: &[],
            current_states: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        crate::resolver::resolve_refs_with_state_and_remote(&mut legacy, &bindings)
            .expect("resolve legacy");
    }

    for (m, l) in managed.iter().zip(legacy.iter()) {
        assert_eq!(
            m.attributes, l.attributes,
            "managed/legacy attribute divergence for {}",
            m.id.name,
        );
        // `dependency_bindings` is the second mutation the legacy
        // pipeline performs; the bridge's writeback contract names
        // both fields, so the equivalence guard does too.
        assert_eq!(
            m.dependency_bindings, l.dependency_bindings,
            "managed/legacy dependency_bindings divergence for {}",
            m.id.name,
        );
    }
}
