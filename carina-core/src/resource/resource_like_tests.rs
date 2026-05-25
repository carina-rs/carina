//! Tests for #3174: `ResourceLike` trait — shared read-only
//! accessors implemented for `Resource`, `Composition`, and
//! `DataSource`.

use std::collections::{BTreeSet, HashSet};

use indexmap::IndexMap;

use crate::resource::{
    Composition, ConcreteValue, DataSource, Directives, ModuleSource, Resource, ResourceId,
    ResourceLike, Signature, Value,
};

fn sample_value(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}

fn deps() -> BTreeSet<String> {
    ["dep_binding".into()].into_iter().collect()
}

fn make_managed() -> Resource {
    Resource::new("aws.s3.Bucket", "b")
        .with_attribute("k", sample_value("v"))
        .with_binding("b")
        .with_dependency_bindings(deps())
        .with_module_source(ModuleSource::module("m", "inst"))
}

fn make_virtual() -> Composition {
    let mut attributes = IndexMap::new();
    attributes.insert("k".to_string(), sample_value("v"));
    Composition {
        id: ResourceId::new("aws.s3.Bucket", "b"),
        signature: Signature {
            arguments: IndexMap::new(),
            attributes,
        },
        binding: Some("b".to_string()),
        dependency_bindings: deps(),
        module_name: "m".to_string(),
        instance: "inst".to_string(),
        quoted_string_attrs: HashSet::new(),
    }
}

fn make_data_source() -> DataSource {
    DataSource {
        id: ResourceId::new("aws.s3.Bucket", "b"),
        attributes: [("k".to_string(), sample_value("v"))].into_iter().collect(),
        directives: Directives::default(),
        binding: Some("b".to_string()),
        dependency_bindings: deps(),
        module_source: Some(ModuleSource::module("m", "inst")),
        quoted_string_attrs: HashSet::new(),
    }
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
    let managed = make_managed();
    let expected_id = managed.id.clone();
    assert_resource_like(&managed, &expected_id, "k", Some("b"), &deps());
}

#[test]
fn composition_implements_resource_like() {
    let v = make_virtual();
    let expected_id = v.id.clone();
    assert_resource_like(&v, &expected_id, "k", Some("b"), &deps());
}

#[test]
fn data_source_implements_resource_like() {
    let ds = make_data_source();
    let expected_id = ds.id.clone();
    assert_resource_like(&ds, &expected_id, "k", Some("b"), &deps());
}

#[test]
fn binding_none_covers_all_arms() {
    // Resource::new() leaves binding = None by default.
    let managed = Resource::new("aws.s3.Bucket", "b");
    assert_eq!(<Resource as ResourceLike>::binding(&managed), None);

    let mut v = make_virtual();
    v.binding = None;
    assert_eq!(<Composition as ResourceLike>::binding(&v), None);

    let ds = DataSource::new("aws.s3.Bucket", "b");
    assert_eq!(<DataSource as ResourceLike>::binding(&ds), None);
}

#[test]
fn resource_like_supports_generic_dispatch() {
    fn first_attribute_key<R: ResourceLike>(r: &R) -> Option<&String> {
        r.attributes().keys().next()
    }

    assert_eq!(first_attribute_key(&make_managed()), Some(&"k".to_string()));
    assert_eq!(first_attribute_key(&make_virtual()), Some(&"k".to_string()));
    assert_eq!(
        first_attribute_key(&make_data_source()),
        Some(&"k".to_string()),
    );
}

#[test]
fn resource_like_is_object_safe() {
    // Using `&dyn ResourceLike` here would force the trait to be
    // object-safe. Read-only accessors with `&self` returning
    // borrowed data satisfy object-safety; lock that in.
    let managed = make_managed();
    let dyn_ref: &dyn ResourceLike = &managed;
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

    let managed = make_managed();
    // `&managed` exercises the blanket `&T` impl; `make_data_source()`
    // exercises the owned-receiver path.
    assert_eq!(ensure_resource_like(&managed), Some("b".into()));
    assert_eq!(ensure_resource_like(make_data_source()), Some("b".into()));
}
