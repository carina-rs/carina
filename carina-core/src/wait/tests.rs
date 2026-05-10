use std::collections::HashMap;

use crate::resource::Value;
use crate::wait::predicate::{AttrPath, WaitPredicate};

#[test]
fn equals_returns_true_when_attribute_matches() {
    let pred = WaitPredicate::Equals {
        attr: AttrPath::single("status"),
        value: Value::String("ISSUED".to_string()),
    };
    let mut attrs = HashMap::new();
    attrs.insert("status".to_string(), Value::String("ISSUED".to_string()));
    assert!(pred.evaluate(&attrs));
}

#[test]
fn equals_returns_false_when_attribute_differs() {
    let pred = WaitPredicate::Equals {
        attr: AttrPath::single("status"),
        value: Value::String("ISSUED".to_string()),
    };
    let mut attrs = HashMap::new();
    attrs.insert(
        "status".to_string(),
        Value::String("PENDING_VALIDATION".to_string()),
    );
    assert!(!pred.evaluate(&attrs));
}

#[test]
fn equals_returns_false_when_attribute_absent() {
    let pred = WaitPredicate::Equals {
        attr: AttrPath::single("status"),
        value: Value::String("ISSUED".to_string()),
    };
    let attrs: HashMap<String, Value> = HashMap::new();
    assert!(!pred.evaluate(&attrs));
}
