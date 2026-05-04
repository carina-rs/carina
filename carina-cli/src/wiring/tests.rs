use super::*;
use carina_core::parser::ParsedFile;

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_ip_protocol_all() {
    // After normalize_desired, ip_protocol "all" becomes a namespaced DSL value.
    // resolve_enum_aliases should resolve the alias "all" -> "-1".
    let mut resource = Resource::with_provider("awscc", "ec2.security_group_egress", "test-rule");
    resource.set_attr(
        "ip_protocol".to_string(),
        Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    assert_eq!(
        resources[0].get_attr("ip_protocol"),
        Some(&Value::String("-1".to_string())),
        "Alias 'all' should be resolved to canonical AWS value '-1'"
    );
}

#[test]
fn test_resolve_enum_aliases_no_alias() {
    // "tcp" has no alias mapping, so it should be converted from DSL enum
    // to its raw form by convert_enum_value but not further changed.
    let mut resource = Resource::with_provider("awscc", "ec2.security_group_egress", "test-rule");
    resource.set_attr(
        "ip_protocol".to_string(),
        Value::String("awscc.ec2.security_group_egress.IpProtocol.tcp".to_string()),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    // "tcp" has no alias, so it remains as the namespaced DSL value
    assert_eq!(
        resources[0].get_attr("ip_protocol"),
        Some(&Value::String(
            "awscc.ec2.security_group_egress.IpProtocol.tcp".to_string()
        )),
    );
}

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_aws_provider() {
    // Same alias resolution should work for the aws provider
    let mut resource = Resource::with_provider("aws", "ec2.security_group_ingress", "test-rule");
    resource.set_attr(
        "ip_protocol".to_string(),
        Value::String("aws.ec2.security_group_ingress.IpProtocol.all".to_string()),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    assert_eq!(
        resources[0].get_attr("ip_protocol"),
        Some(&Value::String("-1".to_string())),
    );
}

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_in_states() {
    // Current states should also have aliases resolved
    let ctx = WiringContext::new(vec![]);
    let id = ResourceId::with_provider("awscc", "ec2.security_group_egress", "test-rule");
    let mut attrs = HashMap::new();
    attrs.insert(
        "ip_protocol".to_string(),
        Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
    );
    let state = State::existing(id.clone(), attrs);
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), state);

    super::resolve_enum_aliases_in_states(&ctx, &mut current_states);

    assert_eq!(
        current_states[&id].attributes.get("ip_protocol"),
        Some(&Value::String("-1".to_string())),
    );
}

#[test]
#[ignore = "requires provider binary for enum alias resolution"]
fn test_resolve_enum_aliases_in_struct_field() {
    // Aliases within struct fields (maps inside lists) should also be resolved
    let mut resource = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg");
    let mut egress_map = IndexMap::new();
    egress_map.insert(
        "ip_protocol".to_string(),
        Value::String("awscc.ec2.SecurityGroup.IpProtocol.all".to_string()),
    );
    egress_map.insert(
        "cidr_ip".to_string(),
        Value::String("0.0.0.0/0".to_string()),
    );
    resource.set_attr(
        "security_group_egress".to_string(),
        Value::List(vec![Value::Map(egress_map)]),
    );

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    if let Value::List(items) = resources[0].get_attr("security_group_egress").unwrap() {
        if let Value::Map(m) = &items[0] {
            assert_eq!(
                m.get("ip_protocol"),
                Some(&Value::String("-1".to_string())),
                "Alias in struct field should be resolved"
            );
            assert_eq!(
                m.get("cidr_ip"),
                Some(&Value::String("0.0.0.0/0".to_string())),
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
    use carina_core::resource::LifecycleConfig;

    let ctx = WiringContext::new(vec![]);

    // Desired resource with normalized DSL enum value (after normalize_desired)
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "test-vpc");
    resource.set_attr(
        "instance_tenancy".to_string(),
        Value::String("awscc.ec2.Vpc.InstanceTenancy.default".to_string()),
    );

    // State with raw AWS value (as returned by provider.read())
    let id = resource.id.clone();
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "instance_tenancy".to_string(),
        Value::String("default".to_string()),
    );
    let state = State::existing(id.clone(), state_attrs);
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), state);

    // Without normalize_state, the differ would see a false diff
    let resources_without = vec![resource.clone()];
    let lifecycles: HashMap<ResourceId, LifecycleConfig> = HashMap::new();
    let schemas = SchemaRegistry::new();
    let saved_attrs = HashMap::new();
    let prev_desired_keys = HashMap::new();
    let orphan_deps = HashMap::new();
    let plan_without = create_plan(
        &resources_without,
        &current_states,
        &lifecycles,
        &schemas,
        &saved_attrs,
        &prev_desired_keys,
        &orphan_deps,
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
        &lifecycles,
        &schemas,
        &saved_attrs,
        &prev_desired_keys,
        &orphan_deps,
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
    use carina_core::resource::LifecycleConfig;
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
    let resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket");

    // State already has the default tags (from a previous apply)
    let id = resource.id.clone();
    let mut state_attrs = HashMap::new();
    let mut tags = IndexMap::new();
    tags.insert(
        "Environment".to_string(),
        Value::String("production".to_string()),
    );
    state_attrs.insert("tags".to_string(), Value::Map(tags));
    let state = State::existing(id.clone(), state_attrs);
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), state);

    let default_tags: IndexMap<String, Value> = {
        let mut m = IndexMap::new();
        m.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );
        m
    };

    // Simulate prev_desired_keys from a previous apply that included "tags"
    // (because merge_default_tags was called correctly in the plan path).
    let mut prev_desired_keys = HashMap::new();
    prev_desired_keys.insert(resource.id.clone(), vec!["tags".to_string()]);

    // Without merge_default_tags, the desired resource has no tags,
    // but state has tags and prev_desired_keys says "tags" was previously desired.
    // The differ sees this as attribute removal → false Update diff.
    let resources_without = vec![resource.clone()];
    let lifecycles: HashMap<ResourceId, LifecycleConfig> = HashMap::new();
    let saved_attrs = HashMap::new();
    let orphan_deps = HashMap::new();
    let plan_without = create_plan(
        &resources_without,
        &current_states,
        &lifecycles,
        &schemas,
        &saved_attrs,
        &prev_desired_keys,
        &orphan_deps,
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
        router.add_normalizer(rt.block_on(factory.create_normalizer(&attrs)));
    }
    let mut resources_with = vec![resource];
    router.merge_default_tags(&mut resources_with, &default_tags, &schemas);

    // After merging, desired now has tags matching state → no diff
    let plan_with = create_plan(
        &resources_with,
        &current_states,
        &lifecycles,
        &schemas,
        &saved_attrs,
        &prev_desired_keys,
        &orphan_deps,
    );
    assert!(
        plan_with.is_empty(),
        "After merge_default_tags, no false diff should occur"
    );
}

#[test]
fn test_resolve_enum_aliases_non_enum_values_unchanged() {
    // Non-DSL-enum strings should not be affected
    let mut resource = Resource::with_provider("awscc", "ec2.SecurityGroup", "test-sg");
    resource.set_attr(
        "group_description".to_string(),
        Value::String("My security group".to_string()),
    );
    resource.set_attr("vpc_id".to_string(), Value::String("vpc-12345".to_string()));

    let mut resources = vec![resource];
    resolve_enum_aliases(&mut resources);

    assert_eq!(
        resources[0].get_attr("group_description"),
        Some(&Value::String("My security group".to_string())),
    );
    assert_eq!(
        resources[0].get_attr("vpc_id"),
        Some(&Value::String("vpc-12345".to_string())),
    );
}

#[test]
fn import_fallback_matches_anonymous_resource_by_name_attribute() {
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::{Resource, ResourceId, Value};
    use carina_core::schema::ResourceSchema;

    // Schema with name_attribute = "bucket_name"
    let bucket_schema = ResourceSchema::new("s3.Bucket").with_name_attribute("bucket_name");
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", bucket_schema);

    // Anonymous resource with hash name but bucket_name = "carina-rs-state"
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "s3_bucket_1d43a664");
    resource.set_attr(
        "bucket_name".to_string(),
        Value::String("carina-rs-state".to_string()),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resource));

    // Import block with the logical name (not the hash)
    let state_blocks = vec![StateBlock::Import {
        to: ResourceId::with_provider("awscc", "s3.Bucket", "carina-rs-state"),
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
        to: ResourceId::with_provider("awscc", "s3.Bucket", "carina-rs-state"),
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
    let mut sso = Resource::with_provider("awscc", "sso.Instance", "carina-rs");
    sso.binding = Some("sso".to_string());

    // Data source referencing `sso.identity_store_id`.
    let mut mizzy = Resource::with_provider("aws", "identitystore.user", "mizzy");
    mizzy.kind = ResourceKind::DataSource;
    mizzy.attributes.insert(
        "identity_store_id".to_string(),
        Value::ResourceRef {
            path: AccessPath::new("sso", "identity_store_id"),
        },
    );
    mizzy.attributes.insert(
        "user_name".to_string(),
        Value::String("gosukenator@gmail.com".into()),
    );

    // current_states after phase 1: sso has been refreshed and its
    // state carries the concrete identity_store_id.
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    let sso_state = State::existing(
        sso.id.clone(),
        HashMap::from([(
            "identity_store_id".to_string(),
            Value::String(identity_store_id.into()),
        )]),
    );
    current_states.insert(sso.id.clone(), sso_state);

    let resolved =
        resolve_data_source_refs_for_refresh(&[sso, mizzy], &current_states, &HashMap::new())
            .expect("resolution should succeed");

    assert_eq!(resolved.len(), 1, "only the data source should be returned");
    let resolved_mizzy = &resolved[0];
    assert_eq!(
        resolved_mizzy.get_attr("identity_store_id"),
        Some(&Value::String(identity_store_id.into())),
        "identity_store_id should be resolved to the concrete state value, \
         not a ResourceRef"
    );
    assert_eq!(
        resolved_mizzy.get_attr("user_name"),
        Some(&Value::String("gosukenator@gmail.com".into())),
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
    use carina_core::resource::{AccessPath, UnknownReason, Value};
    use indexmap::IndexMap;

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    r.attributes
        .insert("group_description".into(), Value::String("web".into()));
    r.attributes.insert(
        "vpc_id".into(),
        Value::Unknown(UnknownReason::UpstreamRef { path: path.clone() }),
    );
    let mut tags: IndexMap<String, Value> = IndexMap::new();
    tags.insert("Name".into(), Value::String("web-sg".into()));
    r.attributes.insert("tags".into(), Value::Map(tags));
    r.attributes.insert(
        "nested_unknown".into(),
        Value::List(vec![Value::Unknown(UnknownReason::UpstreamRef { path })]),
    );
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = super::strip_unknown_attributes(&mut resources);
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

    super::restore_unknown_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "restore must put attributes back at their original index"
    );
    // The restored Unknown values are still typed (not coerced to string).
    assert!(matches!(
        resources[0].get_attr("vpc_id"),
        Some(Value::Unknown(_))
    ));
    match resources[0].get_attr("nested_unknown") {
        Some(Value::List(items)) => {
            assert!(matches!(items[0], Value::Unknown(_)));
        }
        other => panic!(
            "nested_unknown should still be Value::List, got {:?}",
            other
        ),
    }
}

#[test]
fn value_contains_unknown_recurses() {
    use carina_core::resource::{AccessPath, InterpolationPart, UnknownReason, Value};
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    let unknown = || Value::Unknown(UnknownReason::UpstreamRef { path: path.clone() });

    assert!(super::value_contains_unknown(&unknown()));
    assert!(super::value_contains_unknown(&Value::List(vec![unknown()])));
    assert!(super::value_contains_unknown(&Value::Map({
        let mut m = indexmap::IndexMap::new();
        m.insert("k".into(), unknown());
        m
    })));
    assert!(super::value_contains_unknown(&Value::Interpolation(vec![
        InterpolationPart::Expr(unknown()),
    ])));
    assert!(super::value_contains_unknown(&Value::FunctionCall {
        name: "f".into(),
        args: vec![unknown()],
    }));
    assert!(super::value_contains_unknown(&Value::Secret(Box::new(
        unknown()
    ))));

    assert!(!super::value_contains_unknown(&Value::String("x".into())));
    assert!(!super::value_contains_unknown(&Value::Int(1)));
}

#[test]
fn restore_unknown_attributes_after_normalize_injection() {
    // When `normalize_desired` injects new attributes between strip and
    // restore, the originally-stripped Unknown attributes still land at
    // their original `insert_index`; injected attributes end up
    // trailing them. Verifies that `min(len)` clamping doesn't reorder
    // the originals when the post-normalize map has different length.
    use carina_core::resource::{AccessPath, UnknownReason, Value};

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
    r.attributes
        .insert("a".into(), Value::String("a-val".into()));
    r.attributes.insert(
        "b".into(),
        Value::Unknown(UnknownReason::UpstreamRef { path: path.clone() }),
    );
    r.attributes
        .insert("c".into(), Value::String("c-val".into()));
    let mut resources = vec![r];

    let stripped = super::strip_unknown_attributes(&mut resources);
    // After strip: ["a", "c"]. Simulate normalize injecting "z".
    resources[0]
        .attributes
        .insert("z".into(), Value::String("z-val".into()));
    super::restore_unknown_attributes(&mut resources, stripped);

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
    // `Value::Unknown` of any reason.
    use carina_core::resource::{UnknownReason, Value};

    let mut r = carina_core::resource::Resource::new("test.t", "n");
    r.attributes
        .insert("name".into(), Value::String("static".into()));
    r.attributes
        .insert("target_id".into(), Value::Unknown(UnknownReason::ForValue));
    r.attributes.insert(
        "items".into(),
        Value::List(vec![Value::Unknown(UnknownReason::ForKey)]),
    );
    r.attributes
        .insert("index".into(), Value::Unknown(UnknownReason::ForIndex));
    let mut resources = vec![r];
    let order_before: Vec<String> = resources[0].attributes.keys().cloned().collect();

    let stripped = super::strip_unknown_attributes(&mut resources);
    let after_strip: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        after_strip,
        vec!["name".to_string()],
        "all three for-expression Unknown attributes must be stripped"
    );
    assert_eq!(stripped.values().next().unwrap().len(), 3);

    super::restore_unknown_attributes(&mut resources, stripped);
    let order_after: Vec<String> = resources[0].attributes.keys().cloned().collect();
    assert_eq!(
        order_after, order_before,
        "for-expression Unknowns must be restored at their original indices"
    );
    assert!(matches!(
        resources[0].get_attr("target_id"),
        Some(Value::Unknown(UnknownReason::ForValue))
    ));
    assert!(matches!(
        resources[0].get_attr("index"),
        Some(Value::Unknown(UnknownReason::ForIndex))
    ));
}

#[test]
fn value_contains_unknown_covers_for_variants() {
    use carina_core::resource::{UnknownReason, Value};
    assert!(super::value_contains_unknown(&Value::Unknown(
        UnknownReason::ForValue
    )));
    assert!(super::value_contains_unknown(&Value::Unknown(
        UnknownReason::ForKey
    )));
    assert!(super::value_contains_unknown(&Value::Unknown(
        UnknownReason::ForIndex
    )));
    assert!(super::value_contains_unknown(&Value::List(vec![
        Value::Unknown(UnknownReason::ForValue),
    ])));
}
