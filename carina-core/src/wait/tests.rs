use std::collections::HashMap;
use std::collections::HashSet;

use crate::resource::{
    DataSource, ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value, ResolvedResource,
    Resource,
};
use crate::wait::predicate::{AttrPath, WaitPredicate};
use crate::wait::{BindingPattern, WaitTargetLookup, resolve_wait_target};

fn managed(identity: &'static str, binding: Option<&'static str>) -> ResolvedResource {
    let resource: Resource = Resource::with_provider("aws", "test.Resource", identity, None);
    ResolvedResource::new(match binding {
        Some(binding) => resource.with_root_binding(binding),
        None => resource,
    })
}

fn data_source(identity: &'static str, binding: Option<&'static str>) -> DataSource {
    let resource: DataSource = DataSource::with_provider("aws", "test.Lookup", identity, None);
    match binding {
        Some(binding) => resource.with_root_binding(binding),
        None => resource,
    }
}

#[test]
fn target_lookup_prefers_managed_binding_over_data_source_binding() {
    let managed = managed("managed", Some("target"));
    let data_source = data_source("lookup", Some("target"));

    let WaitTargetLookup::Managed(found) = resolve_wait_target(
        "target",
        std::slice::from_ref(&managed),
        std::slice::from_ref(&data_source),
    ) else {
        panic!("managed binding must win over a data-source binding");
    };
    assert_eq!(found.id(), &managed.resolved_id());
}

#[test]
fn target_lookup_finds_data_source_binding() {
    let data_source = data_source("lookup", Some("target"));

    assert!(matches!(
        resolve_wait_target("target", &[], std::slice::from_ref(&data_source)),
        WaitTargetLookup::DataSource
    ));
}

#[test]
fn target_lookup_finds_data_source_identity_without_binding() {
    let data_source = data_source("target", None);

    assert!(matches!(
        resolve_wait_target("target", &[], std::slice::from_ref(&data_source)),
        WaitTargetLookup::DataSource
    ));
}

#[test]
fn target_lookup_prefers_data_source_binding_over_managed_identity() {
    let managed = managed("target", None);
    let data_source = data_source("lookup", Some("target"));

    assert!(matches!(
        resolve_wait_target(
            "target",
            std::slice::from_ref(&managed),
            std::slice::from_ref(&data_source),
        ),
        WaitTargetLookup::DataSource
    ));
}

#[test]
fn target_lookup_prefers_managed_identity_over_data_source_identity() {
    let managed = managed("target", None);
    let data_source = data_source("target", None);

    let WaitTargetLookup::Managed(found) = resolve_wait_target(
        "target",
        std::slice::from_ref(&managed),
        std::slice::from_ref(&data_source),
    ) else {
        panic!("managed identity must win over a data-source identity");
    };
    assert_eq!(found.id(), &managed.resolved_id());
}

#[test]
fn target_lookup_reports_missing_when_no_binding_or_identity_matches() {
    let managed = managed("managed", Some("managed_binding"));
    let data_source = data_source("lookup", Some("lookup_binding"));

    assert!(matches!(
        resolve_wait_target(
            "target",
            std::slice::from_ref(&managed),
            std::slice::from_ref(&data_source),
        ),
        WaitTargetLookup::Missing
    ));
}

#[test]
fn equals_returns_true_when_attribute_matches() {
    let pred = WaitPredicate::equals(
        AttrPath::single("status"),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    )
    .unwrap();
    let mut attrs = HashMap::new();
    attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    );
    assert!(pred.evaluate(&attrs));
}

#[test]
fn equals_returns_false_when_attribute_differs() {
    let pred = WaitPredicate::equals(
        AttrPath::single("status"),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    )
    .unwrap();
    let mut attrs = HashMap::new();
    attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".into())),
    );
    assert!(!pred.evaluate(&attrs));
}

#[test]
fn equals_returns_false_when_attribute_absent() {
    let pred = WaitPredicate::equals(
        AttrPath::single("status"),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    )
    .unwrap();
    let attrs: HashMap<String, Value> = HashMap::new();
    assert!(!pred.evaluate(&attrs));
}

#[test]
fn binding_pattern_variants_construct_and_debug_print() {
    let exact = BindingPattern::Exact("record".to_string());
    assert_eq!(format!("{exact:?}"), r#"Exact("record")"#);

    let children = BindingPattern::ForLoopChildren {
        base: "records".to_string(),
    };
    assert_eq!(
        format!("{children:?}"),
        r#"ForLoopChildren { base: "records" }"#
    );

    let attribute_match = BindingPattern::AttributeMatch {
        resource_type: "route53.RecordSet".to_string(),
        attr: AttrPath::single("name"),
        from: AttrPath::try_new(vec![
            "domain_validation_options".to_string(),
            "resource_record".to_string(),
            "name".to_string(),
        ])
        .unwrap(),
    };
    assert!(format!("{attribute_match:?}").contains("AttributeMatch"));
}

#[test]
fn binding_pattern_compares_and_hashes() {
    let first = BindingPattern::AttributeMatch {
        resource_type: "route53.RecordSet".to_string(),
        attr: AttrPath::single("name"),
        from: AttrPath::single("resource_record_name"),
    };
    let same = BindingPattern::AttributeMatch {
        resource_type: "route53.RecordSet".to_string(),
        attr: AttrPath::single("name"),
        from: AttrPath::single("resource_record_name"),
    };
    let different = BindingPattern::Exact("record".to_string());

    assert_eq!(first, same);
    assert_ne!(first, different);

    let mut set = HashSet::new();
    set.insert(first);
    set.insert(same);
    set.insert(different);
    assert_eq!(set.len(), 2);
}
