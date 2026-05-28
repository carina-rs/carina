//! Integration test for carina#3340: cyclic CFN-style struct
//! definitions modelled via `AttributeType::Ref` + `ResourceSchema::defs`,
//! exercised end-to-end through the differ.
//!
//! The shape mirrors AWS::WAFv2::WebACL `Statement` — the smallest
//! recursive CFN definition that triggered awscc codegen's stack
//! overflow and the entire chain that opens with this PR.
//!
//! What this test pins:
//!
//! 1. A `ResourceSchema` whose `attributes` reference a named def via
//!    `AttributeType::Ref` and whose `defs` map carries the cyclic
//!    struct can be constructed without panicking.
//! 2. `Schema::validate_attr` accepts a well-formed nested value tree
//!    that exercises the cycle at least one hop deep.
//! 3. The differ (`diff`) treats semantically equal cyclic values as
//!    `NoChange`, and surfaces a real change as `Update` with the
//!    correct `changed_attributes` — confirming `type_aware_equal`'s
//!    `Ref` resolution at the walk-site (the load-bearing rule of
//!    the chain).
//! 4. Every non-cyclic `ResourceSchema` constructed via the builder
//!    keeps `defs` empty by default — guards against accidentally
//!    forcing every caller to populate the new field.

use std::collections::{BTreeMap, HashMap};

use carina_core::differ::{Diff, diff};
use carina_core::resource::{ConcreteValue, Resource, ResourceId, State, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, Schema, StructField};

/// Build a minimal WAFv2-WebACL-shaped cyclic schema.
///
/// Top-level attribute `rules` is `List<Struct{ statement: Ref(Statement) }>`.
/// `Statement` is `Struct { and_statement: List<Ref(Statement)> }` —
/// the simplest shape that exercises a true cycle through `Ref`.
fn cyclic_webacl_like_schema() -> ResourceSchema {
    let statement_def = AttributeType::Struct {
        name: "Statement".to_string(),
        fields: vec![StructField::new(
            "and_statement",
            AttributeType::list(AttributeType::Ref("Statement".to_string())),
        )],
    };

    let rule_struct = AttributeType::Struct {
        name: "Rule".to_string(),
        fields: vec![
            StructField::new("name", AttributeType::String),
            StructField::new("statement", AttributeType::Ref("Statement".to_string())),
        ],
    };

    ResourceSchema::new("wafv2.WebACL")
        .attribute(AttributeSchema::new(
            "rules",
            AttributeType::list(rule_struct),
        ))
        .with_def("Statement", statement_def)
}

fn statement_value(child_count: usize) -> Value {
    let children: Vec<Value> = (0..child_count)
        .map(|_| {
            let mut leaf = indexmap::IndexMap::new();
            leaf.insert(
                "and_statement".to_string(),
                Value::Concrete(ConcreteValue::List(Vec::new())),
            );
            Value::Concrete(ConcreteValue::Map(leaf))
        })
        .collect();
    let mut top = indexmap::IndexMap::new();
    top.insert(
        "and_statement".to_string(),
        Value::Concrete(ConcreteValue::List(children)),
    );
    Value::Concrete(ConcreteValue::Map(top))
}

fn rule_value(name: &str, child_count: usize) -> Value {
    let mut rule = indexmap::IndexMap::new();
    rule.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String(name.to_string())),
    );
    rule.insert("statement".to_string(), statement_value(child_count));
    Value::Concrete(ConcreteValue::Map(rule))
}

#[test]
fn cyclic_schema_validates_a_two_level_nested_value() {
    let schema = cyclic_webacl_like_schema();
    let rule_attr_type = &schema.attributes["rules"].attr_type;

    let rules_value = Value::Concrete(ConcreteValue::List(vec![rule_value("BlockBadIPs", 1)]));

    let s = Schema {
        root: AttributeType::String, // unused for this call
        defs: schema.defs.clone(),
    };
    s.validate_attr(rule_attr_type, &rules_value)
        .expect("well-formed cyclic value must validate against schema.defs");
}

#[test]
fn cyclic_schema_rejects_a_string_where_a_list_is_required_under_ref() {
    let schema = cyclic_webacl_like_schema();
    let rule_attr_type = &schema.attributes["rules"].attr_type;

    let mut bad_statement = indexmap::IndexMap::new();
    bad_statement.insert(
        "and_statement".to_string(),
        Value::Concrete(ConcreteValue::String("not a list".to_string())),
    );
    let mut bad_rule = indexmap::IndexMap::new();
    bad_rule.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("X".to_string())),
    );
    bad_rule.insert(
        "statement".to_string(),
        Value::Concrete(ConcreteValue::Map(bad_statement)),
    );
    let rules_value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(bad_rule),
    )]));

    let s = Schema {
        root: AttributeType::String,
        defs: schema.defs.clone(),
    };
    let err = s
        .validate_attr(rule_attr_type, &rules_value)
        .expect_err("string-where-list under Ref must fail validation");
    let text = err.to_string();
    assert!(
        text.to_ascii_lowercase().contains("list") || text.to_ascii_lowercase().contains("type"),
        "diagnostic should mention type/list mismatch, got: {text}"
    );
}

#[test]
fn differ_treats_equal_cyclic_values_as_unchanged() {
    // The load-bearing assertion for carina#3340: the differ's
    // `type_aware_equal` resolves `Ref` at function entry and so
    // semantically equal cyclic values do not surface a phantom diff.
    let schema = cyclic_webacl_like_schema();
    let id = ResourceId::new("wafv2.WebACL", "main");

    let rules_value = Value::Concrete(ConcreteValue::List(vec![rule_value("BlockBadIPs", 2)]));
    let desired = build_resource(&id, &[("rules", rules_value.clone())]);

    let mut current_attrs = HashMap::new();
    current_attrs.insert("rules".to_string(), rules_value);
    let current = State::existing(id.clone(), current_attrs);

    let d = diff(&desired, &current, None, None, Some(&schema));
    assert!(
        matches!(d, Diff::NoChange(_)),
        "equal cyclic values must produce NoChange, got: {d:?}"
    );
}

#[test]
fn differ_surfaces_a_real_change_inside_a_cyclic_value() {
    // Counterpart to the no-phantom case: a real difference one cycle
    // hop deep MUST surface as a single attribute diff. This proves
    // the `Ref` resolution didn't accidentally collapse the inner
    // walk into a no-op.
    let schema = cyclic_webacl_like_schema();
    let id = ResourceId::new("wafv2.WebACL", "main");

    let desired_rules = Value::Concrete(ConcreteValue::List(vec![rule_value("BlockBadIPs", 3)]));
    let desired = build_resource(&id, &[("rules", desired_rules)]);

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![rule_value("BlockBadIPs", 2)])),
    );
    let current = State::existing(id.clone(), current_attrs);

    let d = diff(&desired, &current, None, None, Some(&schema));
    match d {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert_eq!(
                changed_attributes,
                vec!["rules".to_string()],
                "the rules attribute must surface as the changed key (not silently swallowed by the wildcard Ref arm)"
            );
        }
        other => panic!("expected Update, got: {other:?}"),
    }
}

#[test]
fn resource_schema_defs_field_default_is_empty() {
    // Regression guard: every non-cyclic resource schema must continue
    // to construct with an empty `defs` map. A future refactor that
    // accidentally requires `defs` would break every existing builder
    // call.
    let schema = ResourceSchema::new("aws.s3.Bucket")
        .attribute(AttributeSchema::new("bucket", AttributeType::String));
    assert!(schema.defs.is_empty(), "defs default must be empty");
    let _: &BTreeMap<String, AttributeType> = &schema.defs;
}

fn build_resource(id: &ResourceId, attrs: &[(&str, Value)]) -> Resource {
    // The DSL-layer `Resource::new` constructor takes (resource_type,
    // name) and synthesizes an `id`. Use it here so the test does not
    // touch the private `id` field, but immediately overwrite the
    // synthesized id with the one our test owns (same shape, plus an
    // explicit provider).
    let mut r = Resource::new(id.resource_type.clone(), id.name.clone().to_string());
    r.id = id.clone();
    for (k, v) in attrs {
        r = r.with_attribute(k.to_string(), v.clone());
    }
    r
}
