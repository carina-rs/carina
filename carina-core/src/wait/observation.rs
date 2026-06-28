use std::collections::HashMap;

use crate::resource::{ResourceId, Value};
use crate::wait::predicate::{AttrPath, WaitPredicate};

/// Snapshot of what a Wait effect is observing during a single poll
/// iteration. The CLI heartbeat formatter consumes this to render the
/// attribute the wait is actually polling (instead of an arbitrary
/// HashMap entry).
///
/// `watched_attrs` is the set of attributes referenced by the wait's
/// predicate, in evaluation order. Today's predicate AST only contains
/// `Equals { attr, value }` so this is always a single-element list,
/// but the field exists so future predicate variants (`And`, `Or`,
/// `NotEquals`, ...) can grow without breaking consumers.
///
/// `last_attrs` is the raw provider read result; `primary()` projects
/// it through `watched_attrs` for the common heartbeat case.
///
/// Construct via [`WaitObservation::new`] so `watched_attrs` is
/// derived from the predicate. The fields are private to make
/// "watched_attrs that doesn't match any predicate" unrepresentable -
/// see [`WaitObservation::new`].
#[derive(Debug)]
pub struct WaitObservation<'a> {
    binding: &'a str,
    target_id: &'a ResourceId,
    watched_attrs: Vec<&'a AttrPath>,
    last_attrs: &'a HashMap<String, Value>,
}

impl<'a> WaitObservation<'a> {
    /// Build an observation snapshot. `watched_attrs` is derived from
    /// `predicate.watched_attrs()` so consumers cannot pass a set that
    /// disagrees with the predicate. This is the only way to construct
    /// a `WaitObservation` outside the wait module's own tests.
    pub fn new(
        binding: &'a str,
        target_id: &'a ResourceId,
        predicate: &'a WaitPredicate,
        last_attrs: &'a HashMap<String, Value>,
    ) -> Self {
        Self {
            binding,
            target_id,
            watched_attrs: predicate.watched_attrs(),
            last_attrs,
        }
    }

    pub fn binding(&self) -> &str {
        self.binding
    }

    pub fn target_id(&self) -> &ResourceId {
        self.target_id
    }

    pub fn watched_attrs(&self) -> &[&AttrPath] {
        &self.watched_attrs
    }

    pub fn last_attrs(&self) -> &HashMap<String, Value> {
        self.last_attrs
    }

    /// First (path, value) pair where `path` is a watched attribute that
    /// actually appears in the provider's last read result. Returns
    /// `None` when the watched set is empty or none of its keys are
    /// present in `last_attrs` - heartbeat consumers should fall back
    /// to a deterministic display in that case.
    pub fn primary(&self) -> Option<(&AttrPath, &Value)> {
        self.watched_attrs
            .iter()
            .find_map(|&attr| attr.resolve(self.last_attrs).map(|value| (attr, value)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use indexmap::IndexMap;

    use super::WaitObservation;
    use crate::resource::{ConcreteValue, ResourceId, Value};
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    fn string_value(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string()))
    }

    fn equals_predicate(attr: AttrPath, value: &str) -> WaitPredicate {
        WaitPredicate::Equals {
            attr,
            value: string_value(value),
        }
    }

    #[test]
    fn primary_returns_first_watched_attr_present_in_last_attrs() {
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let status = AttrPath::single("status");
        let predicate = equals_predicate(status.clone(), "pending");
        let last_attrs = HashMap::from([
            ("arn".to_string(), string_value("arn:demo")),
            ("status".to_string(), string_value("pending")),
        ]);

        let observation = WaitObservation::new("demo_ready", &target_id, &predicate, &last_attrs);

        let Some((attr, value)) = observation.primary() else {
            panic!("expected primary watched attr");
        };
        assert_eq!(attr, &status);
        assert_eq!(value, last_attrs.get("status").unwrap());
    }

    #[test]
    fn primary_walks_multi_segment_path() {
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let renewal_status =
            AttrPath::try_new(vec!["renewal_summary".into(), "renewal_status".into()]).unwrap();
        let predicate = equals_predicate(renewal_status.clone(), "PENDING");
        let leaf = string_value("PENDING");
        let last_attrs = HashMap::from([(
            "renewal_summary".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "renewal_status".to_string(),
                leaf.clone(),
            )]))),
        )]);

        let observation = WaitObservation::new("demo_ready", &target_id, &predicate, &last_attrs);

        let Some((attr, value)) = observation.primary() else {
            panic!("expected nested watched attr");
        };
        assert_eq!(attr, &renewal_status);
        assert_eq!(value, &leaf);
    }

    #[test]
    fn primary_returns_none_when_no_watched_attr_present() {
        let target_id = ResourceId::with_identity("aws.test.Resource", "demo");
        let xyz = AttrPath::single("xyz");
        let predicate = equals_predicate(xyz, "present");
        let last_attrs = HashMap::from([("arn".to_string(), string_value("arn:demo"))]);

        let observation = WaitObservation::new("demo_ready", &target_id, &predicate, &last_attrs);

        assert!(observation.primary().is_none());
    }
}
