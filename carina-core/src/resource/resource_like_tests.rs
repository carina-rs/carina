//! Tests for #3174: `ResourceLike` trait — shared read-only
//! accessors implemented for `Resource`, `ManagedResource`,
//! `VirtualResource`, and `DataSource`.

use std::collections::BTreeSet;

use crate::resource::{
    ConcreteValue, DataSource, ManagedResource, ModuleSource, Resource, ResourceId, ResourceKind,
    ResourceLike, Value, VirtualResource,
};

fn sample_value(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}

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
        res.virtual_module = Some(("m".into(), "inst".into()));
    }
    res
}

fn assert_resource_like<R: ResourceLike>(
    r: &R,
    expected_id: &ResourceId,
    expected_attr_key: &str,
    expected_binding: Option<&str>,
    expected_deps: &BTreeSet<String>,
) {
    assert_eq!(r.id(), expected_id, "id() mismatch");
    assert!(
        r.attributes().contains_key(expected_attr_key),
        "attributes() missing key {expected_attr_key}",
    );
    assert_eq!(r.binding(), expected_binding, "binding() mismatch");
    assert_eq!(
        r.dependency_bindings(),
        expected_deps,
        "dependency_bindings() mismatch",
    );
}

#[test]
fn resource_implements_resource_like() {
    let res = make_resource(ResourceKind::Managed);
    let deps: BTreeSet<String> = ["dep_binding".into()].into_iter().collect();
    let expected_id = res.id.clone();
    assert_resource_like(&res, &expected_id, "k", Some("b"), &deps);
}

#[test]
fn managed_resource_implements_resource_like() {
    let res = make_resource(ResourceKind::Managed);
    let managed = ManagedResource::try_from(&res).expect("Managed → ManagedResource");
    let deps: BTreeSet<String> = ["dep_binding".into()].into_iter().collect();
    let expected_id = managed.id.clone();
    assert_resource_like(&managed, &expected_id, "k", Some("b"), &deps);
}

#[test]
fn virtual_resource_implements_resource_like() {
    let res = make_resource(ResourceKind::Virtual);
    let v = VirtualResource::try_from(&res).expect("Virtual → VirtualResource");
    let deps: BTreeSet<String> = ["dep_binding".into()].into_iter().collect();
    let expected_id = v.id.clone();
    assert_resource_like(&v, &expected_id, "k", Some("b"), &deps);
}

#[test]
fn data_source_implements_resource_like() {
    let res = make_resource(ResourceKind::DataSource);
    let ds = DataSource::try_from(&res).expect("DataSource → DataSource");
    let deps: BTreeSet<String> = ["dep_binding".into()].into_iter().collect();
    let expected_id = ds.id.clone();
    assert_resource_like(&ds, &expected_id, "k", Some("b"), &deps);
}

#[test]
fn binding_none_covers_all_arms() {
    // Resource::new() leaves binding = None by default.
    let res_managed = Resource::new("aws.s3.Bucket", "b").with_kind(ResourceKind::Managed);
    assert_eq!(<Resource as ResourceLike>::binding(&res_managed), None);

    let managed = ManagedResource::try_from(&res_managed).expect("Managed → ManagedResource");
    assert_eq!(<ManagedResource as ResourceLike>::binding(&managed), None);

    let mut res_virt = Resource::new("aws.s3.Bucket", "b").with_kind(ResourceKind::Virtual);

    res_virt.virtual_module = Some(("m".into(), "i".into()));
    let v = VirtualResource::try_from(&res_virt).expect("Virtual → VirtualResource");
    assert_eq!(<VirtualResource as ResourceLike>::binding(&v), None);

    let res_ds = Resource::new("aws.s3.Bucket", "b").with_kind(ResourceKind::DataSource);
    let ds = DataSource::try_from(&res_ds).expect("DataSource → DataSource");
    assert_eq!(<DataSource as ResourceLike>::binding(&ds), None);
}

#[test]
fn resource_like_supports_generic_dispatch() {
    fn first_attribute_key<R: ResourceLike>(r: &R) -> Option<&String> {
        r.attributes().keys().next()
    }

    let res = make_resource(ResourceKind::Managed);
    assert_eq!(first_attribute_key(&res), Some(&"k".to_string()));

    let managed = ManagedResource::try_from(&res).expect("Managed → ManagedResource");
    assert_eq!(first_attribute_key(&managed), Some(&"k".to_string()));

    let virt_src = make_resource(ResourceKind::Virtual);
    let v = VirtualResource::try_from(&virt_src).expect("Virtual → VirtualResource");
    assert_eq!(first_attribute_key(&v), Some(&"k".to_string()));

    let ds_src = make_resource(ResourceKind::DataSource);
    let ds = DataSource::try_from(&ds_src).expect("DataSource → DataSource");
    assert_eq!(first_attribute_key(&ds), Some(&"k".to_string()));
}

#[test]
fn resource_like_is_object_safe() {
    // Using `&dyn ResourceLike` here would force the trait to be
    // object-safe. Read-only accessors with `&self` returning
    // borrowed data satisfy object-safety; lock that in.
    let res = make_resource(ResourceKind::Managed);
    let dyn_ref: &dyn ResourceLike = &res;
    assert_eq!(dyn_ref.binding(), Some("b"));
    let _ = dyn_ref.attributes();
}

#[test]
fn blanket_impl_makes_references_resource_like() {
    // A `&T` where `T: ResourceLike` is itself `ResourceLike` via
    // the blanket impl. Generic callers can take either by value or
    // by reference.
    fn ensure_resource_like<R: ResourceLike>(r: R) -> Option<String> {
        r.binding().map(str::to_owned)
    }

    let res = make_resource(ResourceKind::Managed);
    assert_eq!(ensure_resource_like(&res), Some("b".into()));

    let managed = ManagedResource::try_from(&res).expect("Managed → ManagedResource");
    assert_eq!(ensure_resource_like(&managed), Some("b".into()));
}
