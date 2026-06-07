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
    let statement_def = AttributeType::struct_(
        "Statement".to_string(),
        vec![StructField::new(
            "and_statement",
            AttributeType::list(AttributeType::ref_("Statement".to_string())),
        )],
    );

    let rule_struct = AttributeType::struct_(
        "Rule".to_string(),
        vec![
            StructField::new("name", AttributeType::string()),
            StructField::new("statement", AttributeType::ref_("Statement".to_string())),
        ],
    );

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
        root: AttributeType::string(), // unused for this call
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
        root: AttributeType::string(),
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
        .attribute(AttributeSchema::new("bucket", AttributeType::string()));
    assert!(schema.defs.is_empty(), "defs default must be empty");
    let _: &BTreeMap<String, AttributeType> = &schema.defs;
}

/// Regression for carina#3345: `ResourceSchema::validate` must route a
/// `Ref`-containing attribute through `Schema::validate_attr` so the
/// enclosing `defs` are in scope, not through the standalone
/// `AttributeType::validate` which can only reject `Ref` with the
/// "reached the standalone validator" sentinel.
///
/// The 7 awscc `s3.Bucket/*` apply failures from the 2026-05-29
/// acceptance run all hit this site (`schema/mod.rs:3053`).
#[test]
fn resource_schema_validate_routes_ref_through_defs() {
    let schema = cyclic_webacl_like_schema();

    let mut attrs = HashMap::new();
    attrs.insert(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![rule_value("BlockBadIPs", 1)])),
    );

    let result = schema.validate(&attrs);
    assert!(
        result.is_ok(),
        "ResourceSchema::validate on a Ref-typed attribute with valid value \
         must succeed, but got: {:?}",
        result.err()
    );
}

/// Regression for carina#3345 Symptom B: `Schema::canonicalize_attr`
/// must drive the `Ref` arm through the schema's `defs` map and
/// canonicalize the resolved type's inner shape. This pins the
/// public Schema-aware canonicalization entry point used by providers
/// when normalising upstream state — the awscc
/// `ec2_vpc_endpoint/{gateway,interface}` plan-verify failures hit
/// this path with `Ref("DnsOptionsSpecification")` and a defs-less
/// caller panicked at `schema/mod.rs:893`.
///
/// The schema here pairs a `Ref` with a leaf type whose
/// canonicalization is observable (`String → StringList` via the
/// `string_or_list_of_strings` collapse #2481/#2510) so a "Ref not
/// followed" regression would leave the raw `String` in place
/// instead of folding it to `StringList`.
#[test]
fn canonicalize_through_defs_resolves_ref_arm_and_walks_resolved_type() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    // `Selectors` def is a struct with a `string_or_list_of_strings`
    // field. `selectors` attribute references it via Ref.
    let selectors_def = AttributeType::struct_(
        "Selectors".to_string(),
        vec![StructField::new(
            "tags",
            AttributeType::union(vec![
                AttributeType::string(),
                AttributeType::list(AttributeType::string()),
            ]),
        )],
    );
    let schema = ResourceSchema::new("test.WithRef")
        .attribute(AttributeSchema::new(
            "selectors",
            AttributeType::ref_("Selectors".to_string()),
        ))
        .with_def("Selectors", selectors_def);

    let attr_type = &schema.attributes["selectors"].attr_type;

    // Construct the value with a bare `String` under `tags`, which
    // `canonicalize_with_type` must collapse to `StringList`
    // *if* the Ref is followed and the resolved struct's field type
    // is consulted.
    let mut payload = indexmap::IndexMap::new();
    payload.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::String("env=prod".to_string())),
    );
    let value = Value::Concrete(ConcreteValue::Map(payload));

    let canon = schema
        .schema_view_for(attr_type.clone())
        .canonicalize(value);

    let Value::Concrete(ConcreteValue::Map(canon_map)) = canon else {
        panic!("canonicalized value must remain a Map");
    };
    let tags = canon_map.get("tags").expect("tags must be present");
    assert!(
        matches!(tags, Value::Concrete(ConcreteValue::StringList(_))),
        "Ref must be resolved against defs and `tags` collapsed to \
         StringList via string_or_list_of_strings (carina#3345 \
         Symptom B). Got: {tags:?}"
    );
}

/// Regression for carina#3345: a `Ref("Missing")` whose name is
/// absent from `defs` must surface a clean `ValidationFailed` with
/// the documented error string rather than a panic or a silent
/// pass-through. `ResourceSchema::validate` is the user-facing
/// entry point that surfaces this.
#[test]
fn dangling_ref_surfaces_clean_validation_error() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.WithDanglingRef").attribute(AttributeSchema::new(
        "broken",
        AttributeType::ref_("Nowhere".to_string()),
    ));
    // No `with_def` call — `defs` stays empty by design.

    let mut attrs = HashMap::new();
    attrs.insert(
        "broken".to_string(),
        Value::Concrete(ConcreteValue::String("anything".to_string())),
    );

    let errors = schema
        .validate(&attrs)
        .expect_err("dangling Ref must fail validation");
    let combined: String = errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        combined.contains("schema reference `Nowhere` is not defined"),
        "expected dangling-Ref message, got: {combined}"
    );
}

/// Regression for carina#3345: a true cyclic def (`Statement` →
/// `List<Ref(Statement)>`) over a finite value must terminate, not
/// blow the stack or hang.  This pins the load-bearing termination
/// invariant for `Schema::validate_attr`'s recursion on Ref-cycles.
#[test]
fn cyclic_ref_terminates_on_finite_value() {
    let schema = cyclic_webacl_like_schema();
    let rule_attr_type = &schema.attributes["rules"].attr_type;

    // 3-level nesting: outer rule contains a statement whose
    // `and_statement` carries two statements, each of which has
    // its own `and_statement: []`. Exercises the cycle twice.
    let rules_value = Value::Concrete(ConcreteValue::List(vec![rule_value("Top", 2)]));

    let s = carina_core::schema::Schema {
        root: AttributeType::string(),
        defs: schema.defs.clone(),
    };
    s.validate_attr(rule_attr_type, &rules_value)
        .expect("finite cyclic value must validate (carina#3345)");
}

/// Regression for carina#3347: `Schema::validate_attr`'s `Map` arm
/// must lift a `Map<Enum, ...>` key into `EnumIdentifier` shape
/// before validating it, mirroring `validate_map`'s pre-existing
/// `key_is_enum` special case (carina#2996). Without this lift, a
/// Map key written as a bare identifier in source surfaces as
/// `StringLiteralExpectedEnum` because the lift wraps the key as a
/// plain `String`.
///
/// The carina#3346 migration routed every production validate call
/// through `Schema::validate_attr`, exposing this latent divergence
/// as a `validate_iam_policy_document` regression in awscc#282 when
/// pulling the new rev.
#[test]
fn schema_validate_attr_map_arm_lifts_enum_key() {
    use carina_core::resource::ConcreteValue;
    use carina_core::schema::{AttributeType, Schema};

    let key_type = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("Op"),
        Some(vec!["eq".to_string(), "neq".to_string()]),
        vec![],
        None,
        None,
    );
    let attr_type = AttributeType::map_with_key(key_type, AttributeType::string());

    let mut payload = indexmap::IndexMap::new();
    payload.insert(
        "eq".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let value = Value::Concrete(ConcreteValue::Map(payload));

    let schema = Schema::flat(attr_type);
    schema
        .validate(&value)
        .expect("a bare-identifier Map key for a Enum key type must validate");

    // Negative case: an unknown enum variant must still fail
    // (guard against a "lift everything to EnumIdentifier and accept"
    // regression).
    let mut bad_payload = indexmap::IndexMap::new();
    bad_payload.insert(
        "unknown".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let bad_value = Value::Concrete(ConcreteValue::Map(bad_payload));
    schema
        .validate(&bad_value)
        .expect_err("an unknown Enum Map key must fail validation");
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
