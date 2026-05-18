use super::*;
use carina_core::parser::ParsedFile;

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_ip_protocol_all() {
    // After normalize_desired, ip_protocol "all" becomes a namespaced DSL value.
    // resolve_enum_aliases should resolve the alias "all" -> "-1".
    let mut resource =
        Resource::with_provider("awscc", "ec2.security_group_egress", "test-rule", None);
    resource.set_attr(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String(
            "awscc.ec2.security_group_egress.IpProtocol.all".to_string(),
        )),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    assert_eq!(
        resources[0].get_attr("ip_protocol"),
        Some(&Value::Concrete(ConcreteValue::String("-1".to_string()))),
        "Alias 'all' should be resolved to canonical AWS value '-1'"
    );
}

#[test]
fn test_resolve_enum_aliases_no_alias() {
    // "tcp" has no alias mapping, so it should be converted from DSL enum
    // to its raw form by convert_enum_value but not further changed.
    let mut resource =
        Resource::with_provider("awscc", "ec2.security_group_egress", "test-rule", None);
    resource.set_attr(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String(
            "awscc.ec2.security_group_egress.IpProtocol.tcp".to_string(),
        )),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    // "tcp" has no alias, so it remains as the namespaced DSL value
    assert_eq!(
        resources[0].get_attr("ip_protocol"),
        Some(&Value::Concrete(ConcreteValue::String(
            "awscc.ec2.security_group_egress.IpProtocol.tcp".to_string()
        ))),
    );
}

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_aws_provider() {
    // Same alias resolution should work for the aws provider
    let mut resource =
        Resource::with_provider("aws", "ec2.security_group_ingress", "test-rule", None);
    resource.set_attr(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String(
            "aws.ec2.security_group_ingress.IpProtocol.all".to_string(),
        )),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    assert_eq!(
        resources[0].get_attr("ip_protocol"),
        Some(&Value::Concrete(ConcreteValue::String("-1".to_string()))),
    );
}

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_in_states() {
    // Current states should also have aliases resolved
    let ctx = WiringContext::new(vec![]);
    let id = ResourceId::with_provider("awscc", "ec2.security_group_egress", "test-rule", None);
    let mut attrs = HashMap::new();
    attrs.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String(
            "awscc.ec2.security_group_egress.IpProtocol.all".to_string(),
        )),
    );
    let state = State::existing(id.clone(), attrs);
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), state);

    super::resolve_enum_aliases_in_states(&ctx, &mut current_states);

    assert_eq!(
        current_states[&id].attributes.get("ip_protocol"),
        Some(&Value::Concrete(ConcreteValue::String("-1".to_string()))),
    );
}

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_in_struct_field() {
    // Aliases within struct fields (maps inside lists) should also be resolved
    let mut resource = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg", None);
    let mut egress_map = IndexMap::new();
    egress_map.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String(
            "awscc.ec2.SecurityGroup.IpProtocol.all".to_string(),
        )),
    );
    egress_map.insert(
        "cidr_ip".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".to_string())),
    );
    resource.set_attr(
        "security_group_egress".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(egress_map),
        )])),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    if let Value::Concrete(ConcreteValue::List(items)) =
        resources[0].get_attr("security_group_egress").unwrap()
    {
        if let Value::Concrete(ConcreteValue::Map(m)) = &items[0] {
            assert_eq!(
                m.get("ip_protocol"),
                Some(&Value::Concrete(ConcreteValue::String("-1".to_string()))),
                "Alias in struct field should be resolved"
            );
            assert_eq!(
                m.get("cidr_ip"),
                Some(&Value::Concrete(ConcreteValue::String(
                    "0.0.0.0/0".to_string()
                ))),
                "Non-alias values should not be changed"
            );
        } else {
            panic!("Expected Map in egress list");
        }
    } else {
        panic!("Expected List for security_group_egress");
    }
}

/// Verify that normalize_state prevents false diffs for enum values in state.
///
/// When state contains raw AWS enum values (e.g., "default") and desired
/// resources have been normalized to DSL enum format (e.g.,
/// "awscc.ec2.Vpc.InstanceTenancy.default"), the differ would see a false
/// diff unless normalize_state is also applied to current states.
///
/// Both the plan path (wiring.rs) and the apply path (apply.rs) must call
/// normalize_state to maintain parity. This test ensures the normalization
/// produces matching values so no false diff occurs.
#[test]
#[ignore = "requires provider binary for state normalization"]
fn test_normalize_state_prevents_false_enum_diff() {
    use carina_core::differ::create_plan;
    use carina_core::resource::Directives;

    let ctx = WiringContext::new(vec![]);

    // Desired resource with normalized DSL enum value (after normalize_desired)
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "test-vpc", None);
    resource.set_attr(
        "instance_tenancy".to_string(),
        Value::Concrete(ConcreteValue::String(
            "awscc.ec2.Vpc.InstanceTenancy.default".to_string(),
        )),
    );

    // State with raw AWS value (as returned by provider.read())
    let id = resource.id.clone();
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "instance_tenancy".to_string(),
        Value::Concrete(ConcreteValue::String("default".to_string())),
    );
    let state = State::existing(id.clone(), state_attrs);
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), state);

    // Without normalize_state, the differ would see a false diff
    let resources_without = vec![resource.clone()];
    let directives_map: HashMap<ResourceId, Directives> = HashMap::new();
    let schemas = SchemaRegistry::new();
    let saved_attrs = HashMap::new();
    let prev_explicit = HashMap::new();
    let orphan_deps = HashMap::new();
    let plan_without = create_plan(
        &resources_without,
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        !plan_without.is_empty(),
        "Without normalize_state, differ should see a false diff"
    );

    // After normalize_state, state values match desired values → no diff
    normalize_state_with_ctx(&ctx, &mut current_states);
    let resources_with = vec![resource];
    let plan_with = create_plan(
        &resources_with,
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        plan_with.is_empty(),
        "After normalize_state, no false diff should occur"
    );
}

/// Verify that merge_default_tags prevents false diffs when default_tags are
/// configured in the provider block.
///
/// When default_tags are set and state already contains those tags (from a
/// previous apply), the differ must not report a diff for the tags. This
/// requires merge_default_tags to be called so the desired resources include
/// the default tags before diffing.
///
/// Both the plan path (wiring.rs) and the apply path (apply.rs) must call
/// merge_default_tags to maintain parity.
#[test]
#[ignore = "requires provider binary for default tags merging"]
fn test_merge_default_tags_prevents_false_diff() {
    use carina_core::differ::create_plan;
    use carina_core::resource::Directives;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    // Build a minimal schema that has a "tags" attribute.
    // merge_default_tags checks for the presence of "tags" in the schema.
    let schema = ResourceSchema::new("s3.Bucket").attribute(AttributeSchema::new(
        "tags",
        AttributeType::map(AttributeType::String),
    ));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);

    // Desired resource without explicit tags
    let resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);

    // State already has the default tags (from a previous apply)
    let id = resource.id.clone();
    let mut state_attrs = HashMap::new();
    let mut tags = IndexMap::new();
    tags.insert(
        "Environment".to_string(),
        Value::Concrete(ConcreteValue::String("production".to_string())),
    );
    state_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags)),
    );
    let state = State::existing(id.clone(), state_attrs);
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), state);

    let default_tags: IndexMap<String, Value> = {
        let mut m = IndexMap::new();
        m.insert(
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("production".to_string())),
        );
        m
    };

    // Simulate prev_explicit from a previous apply that included "tags"
    // (because merge_default_tags was called correctly in the plan path).
    let mut prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields> =
        HashMap::new();
    prev_explicit.insert(
        resource.id.clone(),
        carina_core::explicit::ExplicitFields::Struct {
            children: std::collections::HashMap::from([(
                "tags".to_string(),
                carina_core::explicit::ExplicitFields::Leaf,
            )]),
        },
    );

    // Without merge_default_tags, the desired resource has no tags,
    // but state has tags and prev_explicit says "tags" was previously desired.
    // The differ sees this as attribute removal → false Update diff.
    let resources_without = vec![resource.clone()];
    let directives_map: HashMap<ResourceId, Directives> = HashMap::new();
    let saved_attrs = HashMap::new();
    let orphan_deps = HashMap::new();
    let plan_without = create_plan(
        &resources_without,
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        !plan_without.is_empty(),
        "Without merge_default_tags, differ should see a false diff"
    );

    // After merge_default_tags, desired resource gains the default tags → no diff
    let ctx = WiringContext::new(vec![]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to build tokio runtime");
    let mut router = ProviderRouter::new();
    for factory in ctx.factories() {
        let attrs = IndexMap::new();
        router.add_normalizer(rt.block_on(factory.create_normalizer(None, &attrs)));
    }
    let mut resources_with = vec![resource];
    rt.block_on(router.merge_default_tags(&mut resources_with, &default_tags, &schemas));

    // After merging, desired now has tags matching state → no diff
    let plan_with = create_plan(
        &resources_with,
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        plan_with.is_empty(),
        "After merge_default_tags, no false diff should occur"
    );
}

#[test]
fn test_resolve_enum_aliases_non_enum_values_unchanged() {
    // Non-DSL-enum strings should not be affected
    let mut resource = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg", None);
    resource.set_attr(
        "group_description".to_string(),
        Value::Concrete(ConcreteValue::String("My security group".to_string())),
    );
    resource.set_attr(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-12345".to_string())),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    assert_eq!(
        resources[0].get_attr("group_description"),
        Some(&Value::Concrete(ConcreteValue::String(
            "My security group".to_string()
        ))),
    );
    assert_eq!(
        resources[0].get_attr("vpc_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            "vpc-12345".to_string()
        ))),
    );
}

#[test]
fn import_fallback_matches_anonymous_resource_by_name_attribute() {
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::{ConcreteValue, Resource, ResourceId, Value};
    use carina_core::schema::ResourceSchema;

    // Schema with name_attribute = "bucket_name"
    let bucket_schema = ResourceSchema::new("s3.Bucket").with_name_attribute("bucket_name");
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", bucket_schema);

    // Anonymous resource with hash name but bucket_name = "carina-rs-state"
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "s3_bucket_1d43a664", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs-state".to_string())),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resource));

    // Import block with the logical name (not the hash)
    let state_blocks = vec![StateBlock::Import {
        to: ResourceId::with_provider("awscc", "s3.Bucket", "carina-rs-state", None),
        id: "carina-rs-state".to_string(),
    }];

    add_state_block_effects(&mut plan, &state_blocks, &None, &[], &schemas);

    // Expect only an Import effect (no Create) targeting the anonymous hash name
    let effects = plan.effects();
    assert_eq!(
        effects.len(),
        1,
        "Expected only Import effect, got {effects:?}"
    );
    match &effects[0] {
        Effect::Import { id, identifier } => {
            assert_eq!(
                id.name_str(),
                "s3_bucket_1d43a664",
                "Import should target the anonymous hash name"
            );
            assert_eq!(identifier, "carina-rs-state");
        }
        other => panic!("Expected Import effect, got {other:?}"),
    }
}

#[test]
fn import_fallback_skips_when_already_in_state_by_name_attribute() {
    use carina_core::plan::Plan;
    use carina_core::resource::ResourceId;
    use carina_core::schema::ResourceSchema;
    use carina_state::state::{ResourceState, StateFile};

    let bucket_schema = ResourceSchema::new("s3.Bucket").with_name_attribute("bucket_name");
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", bucket_schema);

    // State has the resource under its anonymous hash name
    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "s3_bucket_1d43a664", "awscc");
    rs.attributes.insert(
        "bucket_name".to_string(),
        serde_json::Value::String("carina-rs-state".to_string()),
    );
    state_file.resources.push(rs);

    let mut plan = Plan::new();
    let state_blocks = vec![StateBlock::Import {
        to: ResourceId::with_provider("awscc", "s3.Bucket", "carina-rs-state", None),
        id: "carina-rs-state".to_string(),
    }];

    add_state_block_effects(&mut plan, &state_blocks, &Some(state_file), &[], &schemas);

    // Already in state (via fallback match) — no Import effect should be emitted
    assert_eq!(
        plan.effects().len(),
        0,
        "Import should be skipped when fallback-matched resource is already in state"
    );
}

/// Regression test for carina#1683: data source input attributes that
/// reference another resource must be resolved against current state
/// *before* being passed to `read_data_source_with_retry`. Without
/// resolution the provider receives a debug-formatted `ResourceRef`
/// string and ships it to the remote API as a literal.
#[test]
fn resolve_data_source_refs_replaces_resource_ref_with_concrete_value() {
    use carina_core::resource::{AccessPath, ResourceKind};

    let identity_store_id = "d-9067c29a4b";

    // Managed resource with a binding — phase 1 would have refreshed it.
    let mut sso = Resource::with_provider("awscc", "sso.Instance", "carina-rs", None);
    sso.binding = Some("sso".to_string());

    // Data source referencing `sso.identity_store_id`.
    let mut mizzy = Resource::with_provider("aws", "identitystore.user", "mizzy", None);
    mizzy.kind = ResourceKind::DataSource;
    mizzy.attributes.insert(
        "identity_store_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("sso", "identity_store_id"),
        }),
    );
    mizzy.attributes.insert(
        "user_name".to_string(),
        Value::Concrete(ConcreteValue::String("gosukenator@gmail.com".into())),
    );

    // current_states after phase 1: sso has been refreshed and its
    // state carries the concrete identity_store_id.
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    let sso_state = State::existing(
        sso.id.clone(),
        HashMap::from([(
            "identity_store_id".to_string(),
            Value::Concrete(ConcreteValue::String(identity_store_id.into())),
        )]),
    );
    current_states.insert(sso.id.clone(), sso_state);

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[sso, mizzy],
        &current_states,
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should succeed");

    assert_eq!(resolved.len(), 1, "only the data source should be returned");
    let resolved_mizzy = &resolved[0];
    assert_eq!(
        resolved_mizzy.get_attr("identity_store_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            identity_store_id.into()
        ))),
        "identity_store_id should be resolved to the concrete state value, \
         not a ResourceRef"
    );
    assert_eq!(
        resolved_mizzy.get_attr("user_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "gosukenator@gmail.com".into()
        ))),
        "literal inputs should pass through untouched"
    );
}

// Two resources with unknown types must surface as two distinct
// `AppError::Validation` entries instead of one joined string, so
// the driver can accumulate diagnostics across validators.
#[test]
fn validate_resources_with_ctx_returns_each_error_as_app_error() {
    let ctx = WiringContext::new(vec![]);

    // Empty provider string sidesteps the "unknown provider, skip"
    // escape hatch (`known_providers` is empty), so each bad resource
    // produces its own "Unknown resource type" entry.
    let r1 = Resource::new("foo.nothing", "first");
    let r2 = Resource::new("bar.nothing", "second");
    let parsed = ParsedFile {
        resources: vec![r1, r2],
        ..ParsedFile::default()
    };

    let provider_ctx = carina_core::parser::ProviderContext::default();
    let errors = validate_resources_with_ctx(&ctx, &parsed, &provider_ctx);
    assert_eq!(errors.len(), 2, "got {errors:?}");
    for err in &errors {
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }
}

// Smoke test for the dependency-chain wrappers: empty input
// exercises each wrapper and pins the `Vec<AppError>` return
// type. A regression back to `Result` fails to compile here.
#[test]
fn dependency_chain_wrappers_return_vec_app_error() {
    let ctx = WiringContext::new(vec![]);
    let mut resources: Vec<Resource> = Vec::new();
    let providers: Vec<ProviderConfig> = Vec::new();

    let errors = resolve_names_with_ctx(&ctx, &mut resources);
    assert!(errors.is_empty(), "resolve_names: got {errors:?}");

    let errors = resolve_attr_prefixes_with_ctx(&ctx, &mut resources);
    assert!(errors.is_empty(), "resolve_attr_prefixes: got {errors:?}");

    let errors = compute_anonymous_identifiers_with_ctx(&ctx, &mut resources, &providers);
    assert!(
        errors.is_empty(),
        "compute_anonymous_identifiers: got {errors:?}",
    );
}

// Smoke test for the module/provider wrappers: empty input exercises
// each wrapper and pins the `Vec<AppError>` return type. A regression
// back to `Result` fails to compile here.
#[test]
fn module_and_provider_wrappers_return_vec_app_error() {
    let ctx = WiringContext::new(vec![]);
    let parsed = ParsedFile::default();
    let base_dir = std::path::Path::new("/tmp/nonexistent-carina-pr3-test");
    let provider_ctx = carina_core::parser::ProviderContext::default();

    let errors = validate_provider_region_with_ctx(&ctx, &parsed);
    assert!(errors.is_empty(), "provider_region: got {errors:?}");

    let errors = validate_module_calls(&parsed, base_dir, &provider_ctx);
    assert!(errors.is_empty(), "module_calls: got {errors:?}");

    let errors = validate_module_attribute_param_types(&ctx, &parsed, base_dir);
    assert!(
        errors.is_empty(),
        "module_attribute_param_types: got {errors:?}",
    );
}

// ----- RFC #2371 stage 2 (#2377): strip-and-restore round trip -----

#[test]
fn strip_and_restore_unknown_attributes_round_trip() {
    use carina_core::resource::{AccessPath, ConcreteValue, DeferredValue, UnknownReason, Value};
    use indexmap::IndexMap;

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    r.attributes.insert(
        "group_description".into(),
        Value::Concrete(ConcreteValue::String("web".into())),
    );
    r.attributes.insert(
        "vpc_id".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        })),
    );
    let mut tags: IndexMap<String, Value> = IndexMap::new();
    tags.insert(
        "Name".into(),
        Value::Concrete(ConcreteValue::String("web-sg".into())),
    );
    r.attributes
        .insert("tags".into(), Value::Concrete(ConcreteValue::Map(tags)));
    r.attributes.insert(
        "nested_unknown".into(),
        Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
            DeferredValue::Unknown(UnknownReason::UpstreamRef { path }),
        )])),
    );
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = super::strip_attributes_matching(&mut resources, &super::value_contains_unknown);
    // After strip: vpc_id and nested_unknown removed.
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        after_strip,
        vec!["group_description".to_string(), "tags".to_string()]
    );
    // Stripped record contains both attributes.
    assert_eq!(stripped.len(), 1);
    let entries = stripped.values().next().unwrap();
    assert_eq!(entries.len(), 2);

    super::restore_stripped_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "restore must put attributes back at their original index"
    );
    // The restored Unknown values are still typed (not coerced to string).
    assert!(matches!(
        resources[0].get_attr("vpc_id"),
        Some(Value::Deferred(DeferredValue::Unknown(_)))
    ));
    match resources[0].get_attr("nested_unknown") {
        Some(Value::Concrete(ConcreteValue::List(items))) => {
            assert!(matches!(
                items[0],
                Value::Deferred(DeferredValue::Unknown(_))
            ));
        }
        other => panic!(
            "nested_unknown should still be Value::Concrete(ConcreteValue::List), got {:?}",
            other
        ),
    }
}

#[test]
fn value_contains_unknown_recurses() {
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
    };
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    let unknown = || {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        }))
    };

    assert!(super::value_contains_unknown(&unknown()));
    assert!(super::value_contains_unknown(&Value::Concrete(
        ConcreteValue::List(vec![unknown()])
    )));
    assert!(super::value_contains_unknown(&Value::Concrete(
        ConcreteValue::Map({
            let mut m = indexmap::IndexMap::new();
            m.insert("k".into(), unknown());
            m
        })
    )));
    assert!(super::value_contains_unknown(&Value::Deferred(
        DeferredValue::Interpolation(vec![InterpolationPart::Expr(unknown()),])
    )));
    assert!(super::value_contains_unknown(&Value::Deferred(
        DeferredValue::FunctionCall {
            name: "f".into(),
            args: vec![unknown()],
        }
    )));
    assert!(super::value_contains_unknown(&Value::Deferred(
        DeferredValue::Secret(Box::new(unknown()))
    )));

    assert!(!super::value_contains_unknown(&Value::Concrete(
        ConcreteValue::String("x".into())
    )));
    assert!(!super::value_contains_unknown(&Value::Concrete(
        ConcreteValue::Int(1)
    )));
}

#[test]
fn restore_unknown_attributes_after_normalize_injection() {
    // When `normalize_desired` injects new attributes between strip and
    // restore, the originally-stripped Unknown attributes still land at
    // their original `insert_index`; injected attributes end up
    // trailing them. Verifies that `min(len)` clamping doesn't reorder
    // the originals when the post-normalize map has different length.
    use carina_core::resource::{AccessPath, ConcreteValue, DeferredValue, UnknownReason, Value};

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    r.attributes.insert(
        "a".into(),
        Value::Concrete(ConcreteValue::String("a-val".into())),
    );
    r.attributes.insert(
        "b".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        })),
    );
    r.attributes.insert(
        "c".into(),
        Value::Concrete(ConcreteValue::String("c-val".into())),
    );
    let mut resources = vec![r];

    let stripped = super::strip_attributes_matching(&mut resources, &super::value_contains_unknown);
    // After strip: ["a", "c"]. Simulate normalize injecting "z".
    resources[0].attributes.insert(
        "z".into(),
        Value::Concrete(ConcreteValue::String("z-val".into())),
    );
    super::restore_stripped_attributes(&mut resources, stripped);

    let order: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order,
        vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "z".to_string()
        ],
        "originals must keep their indices; injected attrs trail them"
    );
}

#[test]
fn strip_and_restore_for_expression_unknowns_round_trip() {
    // The strip-and-restore helpers must cover every `UnknownReason`
    // variant — the WASM provider boundary must never see a
    // `Value::Deferred(DeferredValue::Unknown)` of any reason.
    use carina_core::resource::{ConcreteValue, DeferredValue, UnknownReason, Value};

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    r.attributes.insert(
        "name".into(),
        Value::Concrete(ConcreteValue::String("static".into())),
    );
    r.attributes.insert(
        "target_id".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
    );
    r.attributes.insert(
        "items".into(),
        Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
            DeferredValue::Unknown(UnknownReason::ForKey),
        )])),
    );
    r.attributes.insert(
        "index".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)),
    );
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = super::strip_attributes_matching(&mut resources, &super::value_contains_unknown);
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        after_strip,
        vec!["name".to_string()],
        "all three for-expression Unknown attributes must be stripped"
    );
    assert_eq!(stripped.values().next().unwrap().len(), 3);

    super::restore_stripped_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "for-expression Unknowns must be restored at their original indices"
    );
    assert!(matches!(
        resources[0].get_attr("target_id"),
        Some(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::ForValue
        )))
    ));
    assert!(matches!(
        resources[0].get_attr("index"),
        Some(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::ForIndex
        )))
    ));
}

#[test]
fn value_contains_unknown_covers_for_variants() {
    use carina_core::resource::{ConcreteValue, DeferredValue, UnknownReason, Value};
    assert!(super::value_contains_unknown(&Value::Deferred(
        DeferredValue::Unknown(UnknownReason::ForValue)
    )));
    assert!(super::value_contains_unknown(&Value::Deferred(
        DeferredValue::Unknown(UnknownReason::ForKey)
    )));
    assert!(super::value_contains_unknown(&Value::Deferred(
        DeferredValue::Unknown(UnknownReason::ForIndex)
    )));
    assert!(super::value_contains_unknown(&Value::Concrete(
        ConcreteValue::List(vec![Value::Deferred(DeferredValue::Unknown(
            UnknownReason::ForValue
        )),])
    )));
}

// ----- #2387: strip-and-restore round trip for ResourceRef -----

#[test]
fn strip_and_restore_resource_ref_round_trip() {
    // The strip-and-restore pass must remove any attribute that
    // recursively contains a `Value::Deferred(DeferredValue::ResourceRef)` so the WASM
    // boundary's `core_to_wit_value` never sees one (#2387). This
    // mirrors the stage-2 `Value::Deferred(DeferredValue::Unknown)` round-trip test for the
    // ResourceRef predicate.
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, InterpolationPart, Value, contains_resource_ref,
    };
    use indexmap::IndexMap;

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    let path = AccessPath::with_fields("admins", "group_id", vec![]);
    r.attributes.insert(
        "name".into(),
        Value::Concrete(ConcreteValue::String("static".into())),
    );
    r.attributes.insert(
        "group_id".into(),
        Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
    );
    let mut nested_map: IndexMap<String, Value> = IndexMap::new();
    nested_map.insert(
        "ref".into(),
        Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
    );
    r.attributes.insert(
        "policy".into(),
        Value::Concrete(ConcreteValue::Map(nested_map)),
    );
    r.attributes.insert(
        "label".into(),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: path.clone(),
            })),
        ])),
    );
    r.attributes.insert(
        "joined".into(),
        Value::Deferred(DeferredValue::FunctionCall {
            name: "join".into(),
            args: vec![
                Value::Concrete(ConcreteValue::String(",".into())),
                Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
            ],
        }),
    );
    r.attributes.insert(
        "secret_ref".into(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
            DeferredValue::ResourceRef { path: path.clone() },
        )))),
    );
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = super::strip_attributes_matching(&mut resources, &contains_resource_ref);
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        after_strip,
        vec!["name".to_string()],
        "every attribute that recursively contains a ResourceRef must be stripped"
    );
    let entries = stripped.values().next().expect("one resource stripped");
    assert_eq!(entries.len(), 5);

    super::restore_stripped_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "ResourceRef-bearing attributes must be restored at their original indices"
    );
    // Spot-check that the restored values are still typed (not coerced
    // to a debug-format String — the failure mode #2387 prevents).
    assert!(matches!(
        resources[0].get_attr("group_id"),
        Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
    ));
    assert!(matches!(
        resources[0].get_attr("label"),
        Some(Value::Deferred(DeferredValue::Interpolation(_)))
    ));
    assert!(matches!(
        resources[0].get_attr("joined"),
        Some(Value::Deferred(DeferredValue::FunctionCall { .. }))
    ));
    assert!(matches!(
        resources[0].get_attr("secret_ref"),
        Some(Value::Deferred(DeferredValue::Secret(_)))
    ));
}

#[test]
fn strip_unified_predicate_covers_unknown_and_ref() {
    // The `prepare` pass uses the unified predicate
    // `value_contains_unknown(v) || contains_resource_ref(v)`. Verify
    // it strips both kinds in a single pass, in original order.
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, UnknownReason, Value, contains_resource_ref,
    };

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    let path = AccessPath::with_fields("admins", "group_id", vec![]);
    r.attributes.insert(
        "name".into(),
        Value::Concrete(ConcreteValue::String("static".into())),
    );
    r.attributes.insert(
        "vpc_id".into(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]),
        })),
    );
    r.attributes.insert(
        "group_id".into(),
        Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
    );
    let mut resources = vec![r];

    let stripped = super::strip_attributes_matching(&mut resources, &|v| {
        super::value_contains_unknown(v) || contains_resource_ref(v)
    });
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(after_strip, vec!["name".to_string()]);
    assert_eq!(stripped.values().next().unwrap().len(), 2);

    super::restore_stripped_attributes(&mut resources, stripped);
    assert!(matches!(
        resources[0].get_attr("vpc_id"),
        Some(Value::Deferred(DeferredValue::Unknown(_)))
    ));
    assert!(matches!(
        resources[0].get_attr("group_id"),
        Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
    ));
}

// =====================================================================
// Empty `${}` interpolation rejection (#2487)
//
// The parser accepts `${}` mid-edit (#2480) and the LSP surfaces it as
// a per-location warning. CLI validate / plan / apply must reject it
// explicitly so a buffer with literal `${}` can't reach a provider and
// surface only at the AWS edge.
// =====================================================================

fn parsed_with_attr(attr_name: &str, attr_value: Value) -> ParsedFile {
    let mut r = Resource::new("foo.bar", "x");
    r.attributes.insert(attr_name.to_string(), attr_value);
    ParsedFile {
        resources: vec![r],
        ..ParsedFile::default()
    }
}

#[test]
fn validate_rejects_top_level_empty_interpolation() {
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let value = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("arn:aws:iam::".to_string()),
        InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::EmptyInterpolation,
        ))),
        InterpolationPart::Literal(":root".to_string()),
    ]));
    let parsed = parsed_with_attr("aws", value);

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(errors.len(), 1, "expected one error, got: {:?}", errors);
    let msg = match &errors[0] {
        AppError::Validation(s) => s,
        other => panic!("expected AppError::Validation, got {:?}", other),
    };
    assert!(
        msg.contains("empty interpolation"),
        "message must mention 'empty interpolation'; got: {}",
        msg
    );
    assert!(
        msg.contains("foo.bar.x"),
        "message must include the resource id (provider.type.name); got: {}",
        msg
    );
    assert!(
        msg.contains("aws"),
        "message must name the offending attribute; got: {}",
        msg
    );
}

#[test]
fn validate_rejects_empty_interpolation_inside_secret() {
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let inner = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let parsed = parsed_with_attr(
        "password",
        Value::Deferred(DeferredValue::Secret(Box::new(inner))),
    );

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` wrapped in `secret(...)` must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_inside_function_call() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
    };

    let bad = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let fn_call = Value::Deferred(DeferredValue::FunctionCall {
        name: "join".to_string(),
        args: vec![Value::Concrete(ConcreteValue::String("-".to_string())), bad],
    });
    let parsed = parsed_with_attr("name", fn_call);

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` inside a function-call arg must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_nested_in_map() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
    };
    use indexmap::IndexMap;

    let inner = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".to_string()),
        InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::EmptyInterpolation,
        ))),
    ]));
    let mut map = IndexMap::new();
    map.insert("key".to_string(), inner);
    let parsed = parsed_with_attr("tags", Value::Concrete(ConcreteValue::Map(map)));

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "nested-in-map empty `${{}}` must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_nested_in_list() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
    };

    let inner = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let parsed = parsed_with_attr("items", Value::Concrete(ConcreteValue::List(vec![inner])));

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "nested-in-list empty `${{}}` must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_in_export_value() {
    use carina_core::parser::ParsedExportParam;
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let bad = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let parsed = ParsedFile {
        export_params: vec![ParsedExportParam {
            name: "url".to_string(),
            type_expr: None,
            value: Some(bad),
        }],
        ..ParsedFile::default()
    };

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` in `exports {{ ... }}` value must error; got: {:?}",
        errors
    );
    let msg = match &errors[0] {
        AppError::Validation(s) => s,
        _ => panic!("not a Validation error"),
    };
    assert!(
        msg.contains("exports") && msg.contains("url"),
        "message must name the offending export; got: {}",
        msg
    );
}

#[test]
fn validate_rejects_empty_interpolation_in_attribute_param_default() {
    use carina_core::parser::AttributeParameter;
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let bad = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let parsed = ParsedFile {
        attribute_params: vec![AttributeParameter {
            name: "region".to_string(),
            type_expr: None,
            value: Some(bad),
        }],
        ..ParsedFile::default()
    };

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` in `attributes {{ ... }}` default must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_passes_when_no_empty_interpolation() {
    use carina_core::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};

    // Non-empty interpolation: must not error.
    let value = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".to_string()),
        InterpolationPart::Expr(Value::Concrete(ConcreteValue::String(
            "real-value".to_string(),
        ))),
    ]));
    let parsed = parsed_with_attr("aws", value);

    let errors = validate_no_empty_interpolations(&parsed);
    assert!(
        errors.is_empty(),
        "non-empty interpolation must pass; got: {:?}",
        errors
    );
}

// carina-rs/carina#2594 / #2596: read_with_retry must short-circuit
// to State::not_found when the identifier is None. This guarantees a
// fresh component (no saved state) does not produce an empty-string
// `GetResource` call against the provider.
#[cfg(test)]
mod read_with_retry_identifier_tests {
    use super::*;
    use carina_core::provider::{ProviderResult, ReadRequest};
    use carina_core::resource::{Resource, State};
    use futures::future::BoxFuture;
    use std::sync::Mutex;

    /// A provider whose `read` records every invocation. Used to verify
    /// that `read_with_retry` does *not* call into the provider when
    /// `identifier` is None.
    struct RecordingProvider {
        calls: Mutex<Vec<(ResourceId, Option<String>)>>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }

        fn read(
            &self,
            id: &ResourceId,
            identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.calls
                .lock()
                .unwrap()
                .push((id.clone(), identifier.map(|s| s.to_string())));
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, std::collections::HashMap::new())) })
        }

        fn read_data_source(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: carina_core::provider::CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _request: carina_core::provider::UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: carina_core::provider::DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn read_with_retry_short_circuits_when_identifier_is_none() {
        let provider = RecordingProvider::new();
        let id = ResourceId::with_provider("awscc", "iam.Role", "fresh", None);

        let state = read_with_retry(&provider, &id, None).await.unwrap();

        assert!(
            !state.exists,
            "missing identifier must yield not_found state, not a real read"
        );
        assert_eq!(
            provider.calls.lock().unwrap().len(),
            0,
            "provider.read must NOT be called when identifier is None \
             (regression guard for carina#2594)"
        );
    }

    #[tokio::test]
    async fn read_with_retry_forwards_when_identifier_is_some() {
        let provider = RecordingProvider::new();
        let id = ResourceId::with_provider("awscc", "iam.Role", "existing", None);

        let _state = read_with_retry(&provider, &id, Some("AROABC123"))
            .await
            .unwrap();

        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "provider.read must be called exactly once");
        assert_eq!(calls[0].1.as_deref(), Some("AROABC123"));
    }
}

// ---------------------------------------------------------------------
// carina#3132 PR-1: `expand_same_config_deferred_for` — the pure
// post-refresh expansion the plan path calls. Tested with a hand-built
// post-refresh `current_states` (the documented refresh-phase output);
// `create_plan_from_parsed_with_upstream` invokes this exact function,
// so this is the real plan-path expansion, not a transform in
// isolation (cf. MEMORY "unit-test path ≠ apply path").
//
// `--refresh=false` needs no dedicated case here: the pure function is
// refresh-agnostic (it expands against whatever `current_states` it is
// given — cached-state and live-refresh are the same input shape, both
// covered below). The only refresh-specific code is the child-state
// restore in `create_plan_from_parsed_with_upstream`, which routes the
// new children through the *same* `sf.build_state_for_resource` /
// `State::not_found` path the original resources already use under
// `--refresh=false` (via the shared `children()` filter).
// ---------------------------------------------------------------------
mod expand_same_config_deferred_for_tests {
    use super::*;
    use carina_core::binding_index::WaitAliasSpec;
    use carina_core::parser::{ProviderContext, parse};
    use carina_core::resource::{ConcreteValue, State, Value};
    use std::collections::HashMap;

    /// `let cert` (same-config) + a `for` over its provider-read
    /// `account_ids`, plus a plain independent resource so re-sort
    /// stability has something to preserve order against.
    ///
    /// The loop body uses the **bare** loop variable (`target = id`).
    /// Chained loop-var field access (`opt.resource_record.name`) is
    /// covered separately by
    /// `chained_loop_var_field_access_resolves_post_expansion` below
    /// (carina#3136).
    const SRC: &str = r#"
        let cert = aws.acm.Certificate {
            domain_name       = "registry.example.com"
            validation_method = "DNS"
        }

        aws.ec2.Vpc {
            name       = "anchor"
            cidr_block = "10.0.0.0/16"
        }

        for (_, id) in cert.account_ids {
            aws.sso.Assignment {
                instance_arn = "arn:aws:sso:::instance/ssoins-1"
                target_id    = id
                target_type  = "AWS_ACCOUNT"
            }
        }
    "#;

    /// Build `current_states` as the refresh phase would: the parsed
    /// `cert` resource's own id → an existing State carrying the
    /// provider-read list attribute the loop iterates.
    fn states_with_cert(
        parsed: &ParsedFile,
        attr: &str,
        value: Value,
    ) -> HashMap<ResourceId, State> {
        let cert = parsed
            .resources
            .iter()
            .find(|r| r.binding.as_deref() == Some("cert"))
            .expect("parsed cert resource");
        let mut attrs = HashMap::new();
        attrs.insert(attr.to_string(), value);
        let mut states = HashMap::new();
        states.insert(cert.id.clone(), State::existing(cert.id.clone(), attrs));
        states
    }

    fn account_ids() -> Value {
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("111111111111".into())),
            Value::Concrete(ConcreteValue::String("222222222222".into())),
        ]))
    }

    #[test]
    fn same_config_read_iterable_materializes_in_plan_resources() {
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        assert_eq!(
            parsed.deferred_for_expressions.len(),
            1,
            "loop must be deferred at parse time (iterable is a same-config provider-read)"
        );
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let states = states_with_cert(&parsed, "account_ids", account_ids());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
        )
        .expect("expand");

        let assignments: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("Assignment"))
            .collect();
        assert_eq!(
            assignments.len(),
            2,
            "one concrete Assignment per cert.account_ids entry; got {:?}",
            out.sorted_resources
                .iter()
                .map(|r| r.id.to_string())
                .collect::<Vec<_>>()
        );

        // The bare loop variable must be substituted from the
        // *refreshed* cert value — proving the post-refresh
        // ResolvedBindings view (not the empty pre-refresh map) fed the
        // expansion. This is the carina#3132 fix.
        let mut targets: Vec<String> = assignments
            .iter()
            .filter_map(|r| match r.get_attr("target_id") {
                Some(Value::Concrete(ConcreteValue::String(s))) => Some(s.clone()),
                _ => None,
            })
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            vec!["111111111111".to_string(), "222222222222".to_string()],
            "target_id must be substituted from the refreshed \
             cert.account_ids entries"
        );

        assert!(
            out.residual_deferred_for.is_empty(),
            "resolved loop must leave no residual; got {:?}",
            out.residual_deferred_for
        );
    }

    #[test]
    fn resort_preserves_pre_expansion_relative_order() {
        // carina#3132 highest-risk requirement: appending loop children
        // and re-sorting must not reorder already-planned resources
        // (SimHash / moved-matching stability).
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let pre_order: Vec<String> = sorted.iter().map(|r| r.id.to_string()).collect();
        let states = states_with_cert(&parsed, "account_ids", account_ids());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
        )
        .expect("expand");

        let post_filtered: Vec<String> = out
            .sorted_resources
            .iter()
            .map(|r| r.id.to_string())
            .filter(|id| pre_order.contains(id))
            .collect();
        assert_eq!(
            post_filtered, pre_order,
            "re-sort must preserve the relative order of already-planned resources"
        );
        // The two materialized Assignment children — and only those —
        // are reported as new (none of the pre-expansion ids leak in).
        assert_eq!(
            out.new_child_ids.len(),
            2,
            "new_child_ids must be exactly the materialized loop children"
        );
        assert!(
            out.new_child_ids
                .iter()
                .all(|id| !pre_order.contains(&id.to_string())),
            "new_child_ids must not include any pre-expansion resource"
        );
    }

    #[test]
    fn unresolvable_iterable_stays_deferred_no_misexpansion() {
        // No cert state → iterable genuinely unknowable. The loop must
        // stay in residual (carina#3128 placeholder), NOT mis-expand.
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let empty_states: HashMap<ResourceId, State> = HashMap::new();

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &empty_states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
        )
        .expect("expand");

        assert!(
            out.sorted_resources
                .iter()
                .all(|r| !r.id.resource_type.contains("Assignment")),
            "no Assignment may materialize without a resolvable iterable"
        );
        assert_eq!(
            out.residual_deferred_for.len(),
            1,
            "the unresolvable loop must remain deferred so the validate/\
             plan placeholder still renders"
        );
        assert_eq!(
            out.sorted_resources.len(),
            sorted.len(),
            "resource set unchanged when nothing materialized"
        );
    }

    #[test]
    fn upstream_state_iterable_still_expands_via_unified_view() {
        // Regression guard: the pre-existing `upstream_state` iterable
        // path must keep working now that it flows through the same
        // projected ResolvedBindings (design non-goal: must not regress
        // the upstream path).
        let src = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for account_id in orgs.accounts {
                awscc.sso.Assignment {
                    instance_arn = 'arn:aws:sso:::instance/ssoins-1'
                    target_id    = account_id
                    target_type  = 'AWS_ACCOUNT'
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        assert_eq!(parsed.deferred_for_expressions.len(), 1);
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();

        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("111111111111".to_string())),
                Value::Concrete(ConcreteValue::String("222222222222".to_string())),
            ])),
        );
        let mut remote = HashMap::new();
        remote.insert("orgs".to_string(), orgs_attrs);

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &HashMap::new(),
            &remote,
            &[] as &[WaitAliasSpec],
        )
        .expect("expand");

        let assignments = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("Assignment"))
            .count();
        assert_eq!(
            assignments, 2,
            "upstream_state iterable must still expand via the unified view"
        );
        assert!(out.residual_deferred_for.is_empty());
    }

    #[test]
    fn chained_loop_var_field_access_resolves_post_expansion() {
        // carina#3136 (the flipped carina#3132 PR-1 limitation pin):
        // chained field access on the loop variable
        // (`opt.resource_record.name`) is parsed to
        // `Unknown(ForValuePath { path })` and re-navigated against the
        // real element by `substitute_placeholder` at for-expansion.
        // The loop materializes AND its attributes resolve to concrete
        // values — this is the carina#3132 PR-3 real-registry shape.
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();

        let mut rr = indexmap::IndexMap::new();
        rr.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.r.example.com".into())),
        );
        rr.insert(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.acm-validations.aws.".into())),
        );
        let mut entry = indexmap::IndexMap::new();
        entry.insert(
            "resource_record".to_string(),
            Value::Concrete(ConcreteValue::Map(rr)),
        );
        let dvo = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(entry),
        )]));
        let states = states_with_cert(&parsed, "domain_validation_options", dvo);

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
        )
        .expect("expand");

        let record_sets: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("RecordSet"))
            .collect();
        assert_eq!(
            record_sets.len(),
            1,
            "loop materializes one RecordSet per domain_validation_options entry"
        );
        // carina#3136: the chained loop-var ref is now substituted from
        // the real refreshed element — `name` is the concrete String,
        // not an unresolved ResourceRef.
        assert_eq!(
            record_sets[0].get_attr("name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "_a1.r.example.com".into()
            ))),
            "name must resolve to the refreshed \
             cert.domain_validation_options[0].resource_record.name"
        );
        // And the nested access inside a list literal
        // (`resource_records = [opt.resource_record.value]`) resolves too.
        assert_eq!(
            record_sets[0].get_attr("resource_records"),
            Some(&Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("_a1.acm-validations.aws.".into()))
            ]))),
            "resource_records[0] must resolve to ...resource_record.value"
        );
    }
}
