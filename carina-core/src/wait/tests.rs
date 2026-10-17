use std::collections::HashMap;
use std::collections::HashSet;

use crate::resource::{ConcreteValue, ResourceId, Value};
use crate::schema::SchemaKind;
use crate::wait::predicate::{AttrPath, WaitPredicate};
use crate::wait::{BindingPattern, find_wait_target_with_schema_kind};

#[test]
fn target_lookup_prefers_bindings_then_falls_back_to_identity() {
    let identity_match = ResourceId::with_provider_identity("aws", "test.Resource", "target", None);
    let binding_match =
        ResourceId::with_provider_identity("aws", "test.Resource", "different", None);

    let found = find_wait_target_with_schema_kind(
        "target",
        [
            (&identity_match, None, SchemaKind::DataSource),
            (&binding_match, Some("target"), SchemaKind::Resource),
        ],
    );
    assert_eq!(found, Some((&binding_match, SchemaKind::Resource)));

    let found = find_wait_target_with_schema_kind(
        "target",
        [(&identity_match, None, SchemaKind::DataSource)],
    );
    assert_eq!(found, Some((&identity_match, SchemaKind::DataSource)));
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
