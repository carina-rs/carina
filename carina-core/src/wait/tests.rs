use std::collections::HashMap;
use std::collections::HashSet;

use crate::resource::{ConcreteValue, Value};
use crate::wait::BindingPattern;
use crate::wait::predicate::{AttrPath, WaitPredicate};

#[test]
fn equals_returns_true_when_attribute_matches() {
    let pred = WaitPredicate::Equals {
        attr: AttrPath::single("status"),
        value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    };
    let mut attrs = HashMap::new();
    attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    );
    assert!(pred.evaluate(&attrs));
}

#[test]
fn equals_returns_false_when_attribute_differs() {
    let pred = WaitPredicate::Equals {
        attr: AttrPath::single("status"),
        value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    };
    let mut attrs = HashMap::new();
    attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".to_string())),
    );
    assert!(!pred.evaluate(&attrs));
}

#[test]
fn equals_returns_false_when_attribute_absent() {
    let pred = WaitPredicate::Equals {
        attr: AttrPath::single("status"),
        value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    };
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
        attr: AttrPath {
            segments: vec!["name".to_string()],
        },
        from: AttrPath {
            segments: vec![
                "domain_validation_options".to_string(),
                "resource_record".to_string(),
                "name".to_string(),
            ],
        },
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
