//! Tests for #3173: typed wrappers `ManagedResource`, `VirtualResource`,
//! and `DataSource`, plus their `TryFrom<&Resource>` conversions.
//!
//! No behaviour change — these tests exercise the new types and the
//! fallible conversion from the legacy `Resource` shape.

use std::collections::BTreeSet;

use crate::resource::{
    ConcreteValue, DataSource, Directives, ManagedResource, ModuleSource, Resource, ResourceKind,
    ResourceKindLabel, ResourceKindMismatch, Value, VirtualResource,
};

fn sample_value(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}

/// Build a fully-populated `Resource` of the requested kind.
///
/// Fields that have a builder (`with_*`) are set through it. The two
/// without a builder (`directives`, `prefixes`, `quoted_string_attrs`)
/// are written in-place after construction.
fn make_resource(kind: ResourceKind) -> Resource {
    let deps: BTreeSet<String> = ["dep_binding".into()].into_iter().collect();
    let is_virtual = matches!(kind, ResourceKind::Virtual);

    let mut res = Resource::new("aws.s3.Bucket", "b")
        .with_kind(kind)
        .with_attribute("k", sample_value("v"))
        .with_binding("b")
        .with_dependency_bindings(deps)
        .with_module_source(ModuleSource::module("m", "inst"));
    if is_virtual {
        res.virtual_module = Some(("my_module".into(), "my_instance".into()));
    }

    res.directives = Directives {
        force_delete: true,
        ..Directives::default()
    };
    res.prefixes.insert("k".into(), "pfx-".into());
    res.quoted_string_attrs.insert("k".into());

    res
}

#[test]
fn managed_resource_carries_full_managed_field_set() {
    let res = make_resource(ResourceKind::Managed);
    let managed = ManagedResource::try_from(&res).expect("Managed → ManagedResource");

    assert_eq!(managed.id, res.id);
    assert_eq!(managed.attributes, res.attributes);
    assert_eq!(managed.directives, res.directives);
    assert_eq!(managed.prefixes, res.prefixes);
    assert_eq!(managed.binding, res.binding);
    assert_eq!(managed.dependency_bindings, res.dependency_bindings);
    assert_eq!(managed.module_source, res.module_source);
    assert_eq!(managed.quoted_string_attrs, res.quoted_string_attrs);
}

#[test]
fn virtual_resource_flattens_module_name_and_instance_drops_directives_and_prefixes() {
    let res = make_resource(ResourceKind::Virtual);
    let v = VirtualResource::try_from(&res).expect("Virtual → VirtualResource");

    assert_eq!(v.id, res.id);
    assert_eq!(v.attributes, res.attributes);
    assert_eq!(v.binding, res.binding);
    assert_eq!(v.dependency_bindings, res.dependency_bindings);
    assert_eq!(v.module_name, "my_module");
    assert_eq!(v.instance, "my_instance");
    assert_eq!(v.quoted_string_attrs, res.quoted_string_attrs);
}

#[test]
fn data_source_carries_directives_and_module_source_drops_prefixes() {
    let res = make_resource(ResourceKind::DataSource);
    let ds = DataSource::try_from(&res).expect("DataSource → DataSource");

    assert_eq!(ds.id, res.id);
    assert_eq!(ds.attributes, res.attributes);
    assert_eq!(ds.directives, res.directives);
    assert_eq!(ds.binding, res.binding);
    assert_eq!(ds.dependency_bindings, res.dependency_bindings);
    assert_eq!(ds.module_source, res.module_source);
    assert_eq!(ds.quoted_string_attrs, res.quoted_string_attrs);
}

#[test]
fn try_from_rejects_kind_mismatch_for_managed() {
    let virt = make_resource(ResourceKind::Virtual);
    let err = ManagedResource::try_from(&virt).expect_err("Virtual must not convert to Managed");
    assert_eq!(err.expected, ResourceKindLabel::Managed);
    assert_eq!(err.actual, ResourceKindLabel::Virtual);

    let ds = make_resource(ResourceKind::DataSource);
    let err = ManagedResource::try_from(&ds).expect_err("DataSource must not convert to Managed");
    assert_eq!(err.expected, ResourceKindLabel::Managed);
    assert_eq!(err.actual, ResourceKindLabel::DataSource);
}

#[test]
fn try_from_rejects_kind_mismatch_for_virtual() {
    let managed = make_resource(ResourceKind::Managed);
    let err = VirtualResource::try_from(&managed)
        .expect_err("Managed must not convert to VirtualResource");
    assert_eq!(err.expected, ResourceKindLabel::Virtual);
    assert_eq!(err.actual, ResourceKindLabel::Managed);

    let ds = make_resource(ResourceKind::DataSource);
    let err =
        VirtualResource::try_from(&ds).expect_err("DataSource must not convert to VirtualResource");
    assert_eq!(err.expected, ResourceKindLabel::Virtual);
    assert_eq!(err.actual, ResourceKindLabel::DataSource);
}

#[test]
fn try_from_rejects_kind_mismatch_for_data_source() {
    let managed = make_resource(ResourceKind::Managed);
    let err = DataSource::try_from(&managed).expect_err("Managed must not convert to DataSource");
    assert_eq!(err.expected, ResourceKindLabel::DataSource);
    assert_eq!(err.actual, ResourceKindLabel::Managed);

    let virt = make_resource(ResourceKind::Virtual);
    let err = DataSource::try_from(&virt).expect_err("Virtual must not convert to DataSource");
    assert_eq!(err.expected, ResourceKindLabel::DataSource);
    assert_eq!(err.actual, ResourceKindLabel::Virtual);
}

#[test]
fn kind_mismatch_error_formats_with_both_sides() {
    let err = ResourceKindMismatch {
        expected: ResourceKindLabel::Managed,
        actual: ResourceKindLabel::Virtual,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("Managed") && msg.contains("Virtual"),
        "error message should name both sides, got: {msg}"
    );
}

#[test]
fn resource_kind_label_display_round_trip() {
    assert_eq!(format!("{}", ResourceKindLabel::Managed), "Managed");
    assert_eq!(format!("{}", ResourceKindLabel::Virtual), "Virtual");
    assert_eq!(format!("{}", ResourceKindLabel::DataSource), "DataSource");
}

#[test]
fn resource_kind_projects_to_label_dropping_payload() {
    assert_eq!(ResourceKind::Managed.label(), ResourceKindLabel::Managed);
    assert_eq!(ResourceKind::Virtual.label(), ResourceKindLabel::Virtual,);
    assert_eq!(
        ResourceKind::DataSource.label(),
        ResourceKindLabel::DataSource,
    );
}

// ---------------------------------------------------------------------------
// Compile-time invariant guards
// ---------------------------------------------------------------------------
//
// The point of the typestate split is that fields meaningful only to
// one arm cannot be accessed on a wrong arm. Encoding that invariant
// at runtime would require runtime checks — but with separate structs
// each arm carries only its own fields. The `compile_fail` doctests
// pinning these invariants live on the public structs themselves
// (`VirtualResource` and `DataSource`); if someone re-adds `prefixes`
// (or any other dropped field) to either type, those doctests start
// compiling and CI fails. Keep this comment as the index pointing at
// where the guards actually live.
