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
    // "tcp" has no alias mapping here, so schema-free extraction must not
    // rewrite the namespaced DSL value.
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
        &[],
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        !plan_without.effects().is_empty(),
        "Without normalize_state, differ should see a false diff"
    );

    // After normalize_state, state values match desired values → no diff
    normalize_state_with_ctx(&ctx, &mut current_states);
    let resources_with = vec![resource];
    let plan_with = create_plan(
        &resources_with,
        &[],
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        plan_with.effects().is_empty(),
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
        AttributeType::map(AttributeType::string()),
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
        &[],
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        !plan_without.effects().is_empty(),
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
        &[],
        &current_states,
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        plan_with.effects().is_empty(),
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
    use carina_core::resource::{ConcreteValue, Resource, Value};
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
        to: StateBlockAddress::new("awscc", "s3.Bucket", "carina-rs-state"),
        id: Value::Concrete(ConcreteValue::String("carina-rs-state".to_string())),
    }];

    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let no_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        &None,
        &[],
        &schemas,
        &bindings,
        &no_upstreams,
    );

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
            assert_eq!(
                identifier,
                &Value::Concrete(ConcreteValue::String("carina-rs-state".to_string())),
            );
        }
        other => panic!("Expected Import effect, got {other:?}"),
    }
}

/// Sibling of the Import/Removed routing fix:
/// `moved { from = X 'a', to = X 'b' }` faces the same address-
/// equality bug when the state-resident `a` carries
/// `provider_instance = Some("X")`. Without the root fix
/// `current_states.remove(from)` misses (since the key in the map has
/// the routed id), the moved state is never transferred, and the
/// orphan Delete for `a` is never suppressed — the operator sees both
/// `<- moved` and `- delete` for the same address.
///
/// Coverage shape: state has `a` under routed id; `materialize_moved_states`
/// must (a) actually move the entry under `b`'s routed id, and (b)
/// return a `(from, to)` pair whose ids carry the routing so the
/// downstream `suppress_delete` set in `add_state_block_effects`
/// keys correctly.
#[test]
fn moved_block_resolves_routed_instance_on_from_and_to() {
    use carina_core::resource::{ResourceId, State};
    use carina_state::state::{ResourceState, StateFile};

    // State has the source resource under a routed id.
    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("route53.RecordSet", "old_record", "aws");
    rs.directives.provider_instance = Some("management".to_string());
    state_file.resources.push(rs);

    // Pre-populate `current_states` with the routed `from` id (mirrors
    // what `build_state_for_resource` would produce).
    let routed_from = ResourceId::with_provider(
        "aws",
        "route53.RecordSet",
        "old_record",
        Some("management".to_string()),
    );
    let mut current_states = HashMap::new();
    current_states.insert(routed_from.clone(), State::not_found(routed_from.clone()));

    let mut prev_explicit = HashMap::new();
    let mut saved_attrs = HashMap::new();

    // `moved` block addresses are routing-agnostic by construction
    // (the type makes routing unrepresentable here).
    let state_blocks = vec![StateBlock::Moved {
        from: StateBlockAddress::new("aws", "route53.RecordSet", "old_record"),
        to: StateBlockAddress::new("aws", "route53.RecordSet", "new_record"),
    }];

    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut prev_explicit,
        &mut saved_attrs,
        &state_blocks,
        &Some(state_file),
    );

    // The pair must inherit `from`'s routing so the downstream
    // `suppress_delete` lookup matches the orphan Delete's id.
    assert_eq!(moved_pairs.len(), 1);
    let (from, _to) = &moved_pairs[0];
    assert_eq!(
        from.provider_instance.as_deref(),
        Some("management"),
        "moved.from must inherit routing from the matched state row, got {from:?}",
    );

    // And `current_states` must no longer carry the old key — the
    // state was actually transferred (not silently left under the
    // routed id while a None-routing key was inserted).
    assert!(
        !current_states.contains_key(&routed_from),
        "old routed key must be removed after move",
    );
}

/// Sibling of `import_suppresses_create_when_target_resource_is_routed_to_named_instance`:
/// `removed { from = X 'addr' }` faces the same address-equality
/// mismatch when the state-resident resource has
/// `provider_instance = Some("X")` (originally created via
/// `directives { provider = X }`) but the user-typed `from` parses as
/// `provider_instance = None`. Without the root fix the orphan Delete
/// stays in the plan and the operator sees both `<- removed` and
/// `- delete` for the same address (carina#3324 root cause class).
#[test]
fn removed_block_suppresses_delete_when_state_resource_is_routed_to_named_instance() {
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::ResourceId;
    use carina_state::state::{ResourceState, StateFile};

    let schemas = SchemaRegistry::new();

    // State has a resource that was originally created via
    // `directives { provider = management }` — `provider_instance =
    // Some("management")`.
    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("route53.RecordSet", "r.delegation_ns", "aws");
    rs.directives.provider_instance = Some("management".to_string());
    state_file.resources.push(rs);

    // The orphan Delete effect carries the same routed instance.
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: ResourceId::with_provider(
            "aws",
            "route53.RecordSet",
            "r.delegation_ns",
            Some("management".to_string()),
        ),
        identifier: String::new(),
        directives: carina_core::resource::Directives::default(),
        binding: None,
        dependencies: std::collections::HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
    });

    // `removed` block addresses are routing-agnostic by construction
    // — the newtype makes routing unrepresentable here.
    let state_blocks = vec![StateBlock::Removed {
        from: StateBlockAddress::new("aws", "route53.RecordSet", "r.delegation_ns"),
    }];

    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let no_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        &Some(state_file),
        &[],
        &schemas,
        &bindings,
        &no_upstreams,
    );

    let effects = plan.effects();
    assert_eq!(
        effects.len(),
        1,
        "expected only the Remove effect (orphan Delete must be suppressed \
         when the same address is being removed), got {effects:?}",
    );
    match &effects[0] {
        Effect::Remove { id } => {
            assert_eq!(
                id.provider_instance.as_deref(),
                Some("management"),
                "the remove effect must inherit the routed instance from the \
                 state row so apply-time routing sends the state-removal call \
                 to the correct provider instance",
            );
        }
        other => panic!("expected Remove effect, got {other:?}"),
    }
}

/// carina#3324 regression: an import block targeting a let-bound
/// resource whose `directives { provider = <name> }` routes it to a
/// named provider instance must still suppress the resource's Create
/// effect. Before the fix, the import target was carrying the user-
/// typed `provider_instance = None` while the Create's resource had
/// the directive-routed `provider_instance = Some("management")`, so
/// the `suppress_create` set never matched and the same address
/// appeared as both `<- import` and `+ add` in the plan.
#[test]
fn import_suppresses_create_when_target_resource_is_routed_to_named_instance() {
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::Resource;

    let schemas = SchemaRegistry::new();

    // Let-bound resource with `directives { provider = management }`:
    // the parser stamps `provider_instance = Some("management")` on
    // its ResourceId.
    let resource = Resource::with_provider(
        "aws",
        "route53.RecordSet",
        "r.delegation_ns",
        Some("management".to_string()),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resource));

    // The import block address has no routing slot (`StateBlockAddress`
    // is routing-agnostic by construction). The downstream resolver
    // is responsible for lifting routing from the matched Create.
    let state_blocks = vec![StateBlock::Import {
        to: StateBlockAddress::new("aws", "route53.RecordSet", "r.delegation_ns"),
        id: Value::Concrete(ConcreteValue::String(
            "|hosted-zone-id|registry-dev.carina-rs.dev|NS".to_string(),
        )),
    }];

    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let no_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        &None,
        &[],
        &schemas,
        &bindings,
        &no_upstreams,
    );

    let effects = plan.effects();
    assert_eq!(
        effects.len(),
        1,
        "expected only the Import effect (Create must be suppressed when \
         the same address is being imported), got {effects:?}",
    );
    match &effects[0] {
        Effect::Import { id, .. } => {
            assert_eq!(
                id.provider_instance.as_deref(),
                Some("management"),
                "the import effect must inherit the routed instance from the \
                 matched resource so apply-time routing sends the import call \
                 to the correct provider instance",
            );
        }
        other => panic!("expected Import effect, got {other:?}"),
    }
}

#[test]
fn import_fallback_skips_when_already_in_state_by_name_attribute() {
    use carina_core::plan::Plan;
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
        to: StateBlockAddress::new("awscc", "s3.Bucket", "carina-rs-state"),
        id: Value::Concrete(ConcreteValue::String("carina-rs-state".to_string())),
    }];

    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let no_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        &Some(state_file),
        &[],
        &schemas,
        &bindings,
        &no_upstreams,
    );

    // Already in state (via fallback match) — no Import effect should be emitted
    assert_eq!(
        plan.effects().len(),
        0,
        "Import should be skipped when fallback-matched resource is already in state"
    );
}

#[test]
fn import_claims_resolve_name_attribute_target_and_skip_noop_targets() {
    use carina_core::schema::ResourceSchema;
    use carina_state::state::{ResourceState, StateFile};

    let bucket_schema = ResourceSchema::new("s3.Bucket").with_name_attribute("bucket_name");
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", bucket_schema);

    let mut desired = Resource::with_provider("awscc", "s3.Bucket", "s3_bucket_1d43a664", None);
    desired.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs-state".to_string())),
    );
    let block = StateBlock::Import {
        to: StateBlockAddress::new("awscc", "s3.Bucket", "carina-rs-state"),
        id: Value::Concrete(ConcreteValue::String("carina-rs-state".to_string())),
    };

    let claims = resolve_state_block_claims(
        std::slice::from_ref(&block),
        &None,
        std::slice::from_ref(&desired),
        &schemas,
    );
    assert!(
        claims.claims_to("awscc", "s3.Bucket", "s3_bucket_1d43a664"),
        "name_attribute import target must claim the resolved desired address"
    );

    let mut state_file = StateFile::new();
    state_file.resources.push(ResourceState::new(
        "s3.Bucket",
        "s3_bucket_1d43a664",
        "awscc",
    ));
    let claims = resolve_state_block_claims(
        std::slice::from_ref(&block),
        &Some(state_file),
        std::slice::from_ref(&desired),
        &schemas,
    );
    assert!(
        !claims.claims_to("awscc", "s3.Bucket", "s3_bucket_1d43a664"),
        "already-in-state import target must claim nothing"
    );

    let claims = resolve_state_block_claims(&[block], &None, &[], &schemas);
    assert!(
        !claims.claims_to("awscc", "s3.Bucket", "s3_bucket_1d43a664"),
        "unresolvable import target must claim nothing"
    );
}

#[test]
fn test_materialize_moved_states_warns_on_missing_from() {
    use carina_state::state::{ResourceState, StateFile};

    let state_blocks = vec![StateBlock::Moved {
        from: StateBlockAddress::new("awscc", "s3.Bucket", "old_bucket"),
        to: StateBlockAddress::new("awscc", "s3.Bucket", "new_bucket"),
    }];
    let mut warnings = Vec::new();
    let moved_pairs = materialize_moved_states_with_warning_sink(
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &state_blocks,
        &Some(StateFile::new()),
        &mut |warning| warnings.push(warning),
    );
    assert!(moved_pairs.is_empty());
    assert_eq!(
        warnings,
        vec![
            "warning: moved block from awscc.s3.Bucket 'old_bucket' to awscc.s3.Bucket 'new_bucket' was not applied: old_bucket not found in state"
                .to_string()
        ],
    );

    let mut state_file = StateFile::new();
    state_file
        .resources
        .push(ResourceState::new("s3.Bucket", "new_bucket", "awscc"));
    let mut warnings = Vec::new();
    let moved_pairs = materialize_moved_states_with_warning_sink(
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &state_blocks,
        &Some(state_file),
        &mut |warning| warnings.push(warning),
    );
    assert!(moved_pairs.is_empty());
    assert!(
        warnings.is_empty(),
        "already-applied from-absent/to-present moved block stays silent"
    );
}

fn bucket_id(name: &str) -> ResourceId {
    ResourceId::with_provider("awscc", "s3.Bucket", name, None)
}

fn bucket_resource(name: &str) -> Resource {
    Resource::with_provider("awscc", "s3.Bucket", name, None)
}

fn bucket_state_file(names: &[&str]) -> carina_state::state::StateFile {
    use carina_state::state::{ResourceState, StateFile};

    let mut state_file = StateFile::new();
    for name in names {
        state_file
            .resources
            .push(ResourceState::new("s3.Bucket", *name, "awscc"));
    }
    state_file
}

fn assert_collision_contains(result: Result<(), AppError>, needle: &str) {
    let err = result.expect_err("expected collision error");
    assert!(
        matches!(err, AppError::Validation(_)),
        "collision errors must be validation errors, got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "expected error to contain {needle:?}, got {message:?}"
    );
}

#[tokio::test]
async fn test_plan_fails_on_moved_from_colliding_with_desired() {
    use carina_core::parser::{ProviderContext, parse};

    let source = r#"
let live = awscc.s3.Bucket {
    bucket_name = "live"
}

moved {
    from = awscc.s3.Bucket "live"
    to   = awscc.s3.Bucket "renamed"
}
"#;
    let parsed = parse(source, &ProviderContext::default()).expect("parse fixture");
    let state_file = Some(bucket_state_file(&["live"]));
    let StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = resolve_state_blocks(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let tmp = tempfile::tempdir().expect("tempdir");

    let err = match create_plan_from_parsed_with_upstream(
        &parsed,
        &state_file,
        false,
        &HashMap::new(),
        &state_block_claims,
        &resolved_state_block_targets,
        tmp.path(),
    )
    .await
    {
        Ok(_) => panic!("plan must fail before producing a green move plan"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        matches!(err, AppError::Validation(_)),
        "plan collision errors must be validation errors, got {err:?}"
    );
    assert!(
        message.contains(
            "moved/rename pair from awscc.s3.Bucket live collides with a desired resource"
        ),
        "unexpected plan error: {message}"
    );
}

#[tokio::test]
async fn test_plan_fails_on_removed_from_colliding_with_expanded_loop_child() {
    use carina_core::parser::{ProviderContext, parse};
    use carina_state::state::{ResourceState, StateFile};

    let source = r#"
let cert = aws.acm.Certificate {
    domain_name       = "r.example.com"
    validation_method = "DNS"
}

let records = for (_, opt) in cert.domain_validation_options {
    aws.route53.RecordSet {
        name             = opt.resource_record.name
        type             = "CNAME"
        resource_records = [opt.resource_record.value]
    }
}

removed {
    from = aws.route53.RecordSet "records[0]"
}
"#;
    let parsed = parse(source, &ProviderContext::default()).expect("parse removed fixture");
    let state_file = {
        let mut sf = StateFile::new();
        sf.resources.push(
            ResourceState::new("acm.Certificate", "cert", "aws")
                .with_identifier("cert-arn")
                .with_attribute(
                    "domain_validation_options",
                    serde_json::json!([
                        {
                            "resource_record": {
                                "name": "_a1.r.example.com",
                                "value": "_a1.acm-validations.aws."
                            }
                        }
                    ]),
                ),
        );
        sf.resources.push(
            ResourceState::new("route53.RecordSet", "records[0]", "aws")
                .with_identifier("record-set-id"),
        );
        Some(sf)
    };
    let StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = resolve_state_blocks(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let tmp = tempfile::tempdir().expect("tempdir");

    let err = match create_plan_from_parsed_with_upstream(
        &parsed,
        &state_file,
        false,
        &HashMap::new(),
        &state_block_claims,
        &resolved_state_block_targets,
        tmp.path(),
    )
    .await
    {
        Ok(_) => panic!("plan must fail once deferred-for children are in the desired set"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("removed block from aws.route53.RecordSet"),
        "unexpected plan error: {message}"
    );
    assert!(
        message.contains("collides with desired resource"),
        "unexpected plan error: {message}"
    );
}

#[test]
fn test_plan_fails_on_two_moves_to_same_target() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["old_a", "old_b"]));
    let moved_pairs = vec![
        (bucket_id("old_a"), bucket_id("target")),
        (bucket_id("old_b"), bucket_id("target")),
    ];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &desired,
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "two moved/rename pairs resolve to the same target awscc.s3.Bucket target",
    );
}

#[test]
fn test_plan_fails_on_two_moves_from_same_source() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["old"]));
    let moved_pairs = vec![
        (bucket_id("old"), bucket_id("target_a")),
        (bucket_id("old"), bucket_id("target_b")),
    ];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &desired,
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "two moved/rename pairs share the same source awscc.s3.Bucket old",
    );
}

#[test]
fn test_plan_fails_on_removed_from_colliding_with_desired() {
    let desired = vec![bucket_resource("live")];
    let state_file = Some(bucket_state_file(&["live"]));
    let blocks = vec![StateBlock::Removed {
        from: StateBlockAddress::new("awscc", "s3.Bucket", "live"),
    }];
    let resolution = resolve_state_blocks(
        &blocks,
        &state_file,
        &desired,
        &carina_core::schema::SchemaRegistry::new(),
    );

    assert_collision_contains(
        validate_plan_time_state_block_collisions(&desired, &[], &resolution.targets, &state_file),
        "removed block from awscc.s3.Bucket live collides with desired resource awscc.s3.Bucket live",
    );
}

#[test]
fn test_plan_fails_on_move_onto_occupied_state_entry() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["old", "occupied"]));
    let moved_pairs = vec![(bucket_id("old"), bucket_id("occupied"))];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &desired,
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "moved/rename pair awscc.s3.Bucket old -> awscc.s3.Bucket occupied would overwrite an existing state entry",
    );
}

#[test]
fn test_plan_allows_from_absent_to_present_idempotent_noop() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["already_moved"]));
    let state_blocks = vec![StateBlock::Moved {
        from: StateBlockAddress::new("awscc", "s3.Bucket", "old_name"),
        to: StateBlockAddress::new("awscc", "s3.Bucket", "already_moved"),
    }];
    let mut warnings = Vec::new();

    let moved_pairs = materialize_moved_states_with_warning_sink(
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &state_blocks,
        &state_file,
        &mut |warning| warnings.push(warning),
    );
    assert!(
        moved_pairs.is_empty(),
        "already-applied moved block must not produce a move pair"
    );
    assert!(
        warnings.is_empty(),
        "already-applied moved block must stay silent"
    );

    validate_plan_time_state_block_collisions(
        &desired,
        &moved_pairs,
        &ResolvedStateBlockTargets::default(),
        &state_file,
    )
    .expect("from-absent/to-present no-op must not error");
}

#[test]
fn test_plan_fails_on_synthesized_rename_colliding_with_moved_to() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["operator_source", "anonymous_source"]));
    let operator_moved_pair = (bucket_id("operator_source"), bucket_id("named"));
    let synthesized_rename_pair = (bucket_id("anonymous_source"), bucket_id("named"));
    let moved_pairs = vec![operator_moved_pair, synthesized_rename_pair];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &desired,
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "two moved/rename pairs resolve to the same target awscc.s3.Bucket named",
    );
}

#[test]
fn test_plan_fails_on_orphan_self_move() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["orphan"]));
    let moved_pairs = vec![(bucket_id("orphan"), bucket_id("orphan"))];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &desired,
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "moved block from and to name the same address awscc.s3.Bucket orphan: a self-move cannot transfer state",
    );
}

#[test]
fn test_plan_fails_on_rotation_shape() {
    let desired = vec![bucket_resource("b"), bucket_resource("c")];
    let state_file = Some(bucket_state_file(&["a", "b"]));
    let moved_pairs = vec![
        (bucket_id("a"), bucket_id("b")),
        (bucket_id("b"), bucket_id("c")),
    ];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &desired,
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "moved/rename pair from awscc.s3.Bucket b collides with a desired resource",
    );
}

struct AssociationCreateOnlyFactory;

impl ProviderFactory for AssociationCreateOnlyFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC association create-only test provider"
    }

    fn provider_config_attribute_types(
        &self,
    ) -> HashMap<String, carina_core::schema::AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<Box<dyn carina_core::provider::Provider>>,
    > {
        Box::pin(async {
            Ok(Box::new(MockProvider::new()) as Box<dyn carina_core::provider::Provider>)
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> carina_core::provider::BoxFuture<'_, Box<dyn carina_core::provider::ProviderNormalizer>>
    {
        Box::pin(async {
            Box::new(carina_core::provider::NoopNormalizer)
                as Box<dyn carina_core::provider::ProviderNormalizer>
        })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

        vec![
            ResourceSchema::new("s3.Bucket")
                .with_name_attribute("bucket_name")
                .attribute(
                    AttributeSchema::new("bucket_name", AttributeType::string()).create_only(),
                ),
            ResourceSchema::new("ec2.SubnetRouteTableAssociation")
                .attribute(
                    AttributeSchema::new("route_table_id", AttributeType::string()).create_only(),
                )
                .attribute(
                    AttributeSchema::new("subnet_id", AttributeType::string()).create_only(),
                ),
        ]
    }
}

fn route_table_state(binding: &str, id: &str) -> carina_state::state::ResourceState {
    let mut state = carina_state::state::ResourceState::new("ec2.RouteTable", binding, "awscc");
    state.binding = Some(binding.to_string());
    state
        .attributes
        .insert("id".to_string(), serde_json::Value::String(id.to_string()));
    state
}

fn association_state(
    name: &str,
    route_table_id: &str,
    subnet_id: &str,
) -> carina_state::state::ResourceState {
    let mut state =
        carina_state::state::ResourceState::new("ec2.SubnetRouteTableAssociation", name, "awscc");
    state.attributes.insert(
        "route_table_id".to_string(),
        serde_json::Value::String(route_table_id.to_string()),
    );
    state.attributes.insert(
        "subnet_id".to_string(),
        serde_json::Value::String(subnet_id.to_string()),
    );
    state
}

fn desired_association(name: &str, route_table_binding: &str, subnet_id: &str) -> Resource {
    use carina_core::resource::AccessPath;

    let mut resource =
        Resource::with_provider("awscc", "ec2.SubnetRouteTableAssociation", name, None);
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new(route_table_binding, "id"),
        }),
    );
    resource.set_attr(
        "subnet_id".to_string(),
        Value::Concrete(ConcreteValue::String(subnet_id.to_string())),
    );
    resource
}

fn bucket_state(name: &str, bucket_name: &str) -> carina_state::state::ResourceState {
    let mut state = carina_state::state::ResourceState::new("s3.Bucket", name, "awscc");
    state.attributes.insert(
        "bucket_name".to_string(),
        serde_json::Value::String(bucket_name.to_string()),
    );
    state
}

fn desired_bucket(name: &str, bucket_name: &str) -> Resource {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", name, None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String(bucket_name.to_string())),
    );
    resource
}

#[test]
fn import_claimed_name_attribute_target_excludes_desired_from_reconciliation() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new(vec![Box::new(AssociationCreateOnlyFactory)]);
    let old_name = "s3_bucket_1d43a664";
    let desired_name = "s3_bucket_desired";
    let bucket_name = "carina-rs-state";
    let mut state_file = StateFile::new();
    state_file
        .resources
        .push(bucket_state(old_name, bucket_name));
    let mut resources = vec![desired_bucket(desired_name, bucket_name)];
    let state_blocks = vec![StateBlock::Import {
        to: StateBlockAddress::new("awscc", "s3.Bucket", bucket_name),
        id: Value::Concrete(ConcreteValue::String(bucket_name.to_string())),
    }];

    let claims = resolve_state_block_claims(
        &state_blocks,
        &Some(state_file.clone()),
        &resources,
        ctx.schemas(),
    );
    assert!(
        claims.claims_to("awscc", "s3.Bucket", desired_name),
        "name_attribute import target must claim the resolved desired resource"
    );

    reconcile_anonymous_identifiers_with_ctx(&ctx, &mut resources, &mut state_file, &claims);

    assert_eq!(
        resources[0].id.name_str(),
        desired_name,
        "import-claimed desired resource must not adopt the matching old hash name"
    );
    assert!(
        state_file
            .find_resource("awscc", "s3.Bucket", old_name)
            .is_some(),
        "old hash state entry must stay put for the orphan path"
    );
}

fn assert_claimed_association_stays_orphaned_after_reconcile() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new(vec![Box::new(AssociationCreateOnlyFactory)]);
    let old_name = "ec2_subnet_route_table_association_11111111";
    let desired_name = "ec2_subnet_route_table_association_aaaaaaaa";
    let mut state_file = StateFile::new();
    state_file
        .resources
        .push(route_table_state("private_rtb", "rtb-private"));
    state_file
        .resources
        .push(association_state(old_name, "rtb-private", "subnet-a"));
    let mut resources = vec![desired_association(desired_name, "private_rtb", "subnet-a")];
    let state_blocks = vec![StateBlock::Removed {
        from: StateBlockAddress::new("awscc", "ec2.SubnetRouteTableAssociation", old_name),
    }];
    let claims = resolve_state_block_claims(
        &state_blocks,
        &Some(state_file.clone()),
        &resources,
        ctx.schemas(),
    );

    assert!(
        claims.claims_from("awscc", "ec2.SubnetRouteTableAssociation", old_name),
        "effective removed block must claim its state entry"
    );
    reconcile_anonymous_identifiers_with_ctx(&ctx, &mut resources, &mut state_file, &claims);

    assert_eq!(
        resources[0].id.name_str(),
        desired_name,
        "claimed state entry must not be rebound to the desired resource"
    );
    assert!(
        state_file
            .find_resource("awscc", "ec2.SubnetRouteTableAssociation", old_name)
            .is_some(),
        "claimed state entry must survive under its old name for the orphan path"
    );
}

#[test]
fn claimed_state_entry_survives_orphan_reconcile_for_destroy_and_refresh() {
    // Both command shapes reach the same
    // `reconcile_anonymous_identifiers_with_ctx` exclusion seam:
    // destroy sends the claimed entry through the orphan-delete path under
    // its old name, while state refresh refreshes it in place under that
    // old name.
    assert_claimed_association_stays_orphaned_after_reconcile();
}

#[test]
fn reconcile_anonymous_identifiers_with_ctx_resolves_deferred_create_only_from_state_bindings() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new(vec![Box::new(AssociationCreateOnlyFactory)]);
    let mut state_file = StateFile::new();
    state_file
        .resources
        .push(route_table_state("private_rtb", "rtb-private"));
    state_file
        .resources
        .push(route_table_state("public_rtb", "rtb-public"));
    state_file.resources.push(association_state(
        "ec2_subnet_route_table_association_11111111",
        "rtb-private",
        "subnet-a",
    ));
    state_file.resources.push(association_state(
        "ec2_subnet_route_table_association_22222222",
        "rtb-public",
        "subnet-a",
    ));
    state_file.resources.push(association_state(
        "ec2_subnet_route_table_association_33333333",
        "rtb-private",
        "subnet-c",
    ));

    let mut resources = vec![
        desired_association(
            "ec2_subnet_route_table_association_aaaaaaaa",
            "private_rtb",
            "subnet-a",
        ),
        desired_association(
            "ec2_subnet_route_table_association_bbbbbbbb",
            "public_rtb",
            "subnet-a",
        ),
        desired_association(
            "ec2_subnet_route_table_association_cccccccc",
            "private_rtb",
            "subnet-c",
        ),
    ];

    reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        &mut resources,
        &mut state_file,
        &carina_core::identifier::StateBlockClaims::empty(),
    );

    let names: Vec<_> = resources
        .iter()
        .map(|resource| resource.id.name_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "ec2_subnet_route_table_association_11111111",
            "ec2_subnet_route_table_association_22222222",
            "ec2_subnet_route_table_association_33333333",
        ],
        "desired anonymous associations must adopt state names by resolved create-only values",
    );
}

#[test]
fn test_stale_moved_block_releases_claims() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new(vec![Box::new(AssociationCreateOnlyFactory)]);
    let mut state_file = StateFile::new();
    state_file
        .resources
        .push(route_table_state("private_rtb", "rtb-private"));
    state_file.resources.push(association_state(
        "ec2_subnet_route_table_association_11111111",
        "rtb-private",
        "subnet-a",
    ));

    let mut resources = vec![desired_association(
        "ec2_subnet_route_table_association_aaaaaaaa",
        "private_rtb",
        "subnet-a",
    )];
    let state_blocks = vec![StateBlock::Moved {
        from: StateBlockAddress::new("awscc", "ec2.SubnetRouteTableAssociation", "does_not_exist"),
        to: StateBlockAddress::new(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            "ec2_subnet_route_table_association_aaaaaaaa",
        ),
    }];
    let claims = resolve_state_block_claims(
        &state_blocks,
        &Some(state_file.clone()),
        &resources,
        ctx.schemas(),
    );

    assert!(
        !claims.claims_to(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            "ec2_subnet_route_table_association_aaaaaaaa",
        ),
        "ineffective moved block must not pin its to address"
    );
    reconcile_anonymous_identifiers_with_ctx(&ctx, &mut resources, &mut state_file, &claims);

    assert_eq!(
        resources[0].id.name_str(),
        "ec2_subnet_route_table_association_11111111",
        "stale moved block must release claims so meaning-based matching can preserve the no-op"
    );
}

#[test]
fn moved_blocks_are_honored_before_heuristic_reconciliation_for_five_renames() {
    use carina_core::resource::{ResourceId, State};
    use carina_state::state::StateFile;

    let ctx = WiringContext::new(vec![Box::new(AssociationCreateOnlyFactory)]);
    let mut state_file = StateFile::new();
    let mut resources = Vec::new();
    let mut state_blocks = Vec::new();
    let mut current_states = HashMap::new();
    let mut old_names = Vec::new();
    let mut desired_names = Vec::new();

    for idx in 0..5 {
        let binding = format!("rtb_{idx}");
        let route_table_id = format!("rtb-{idx}");
        let subnet_id = format!("subnet-{idx}");
        let old_name = format!("ec2_subnet_route_table_association_old_{idx:08x}");
        let desired_name = format!("ec2_subnet_route_table_association_new_{idx:08x}");

        state_file
            .resources
            .push(route_table_state(&binding, &route_table_id));
        state_file
            .resources
            .push(association_state(&old_name, &route_table_id, &subnet_id));
        resources.push(desired_association(&desired_name, &binding, &subnet_id));
        state_blocks.push(StateBlock::Moved {
            from: StateBlockAddress::new("awscc", "ec2.SubnetRouteTableAssociation", &old_name),
            to: StateBlockAddress::new("awscc", "ec2.SubnetRouteTableAssociation", &desired_name),
        });

        let old_id =
            ResourceId::with_provider("awscc", "ec2.SubnetRouteTableAssociation", &old_name, None);
        current_states.insert(old_id.clone(), State::not_found(old_id));
        old_names.push(old_name);
        desired_names.push(desired_name);
    }

    let claims = resolve_state_block_claims(
        &state_blocks,
        &Some(state_file.clone()),
        &resources,
        ctx.schemas(),
    );
    reconcile_anonymous_identifiers_with_ctx(&ctx, &mut resources, &mut state_file, &claims);
    assert_eq!(
        resources
            .iter()
            .map(|resource| resource.id.name_str().to_string())
            .collect::<Vec<_>>(),
        desired_names,
        "heuristics must not re-key desired resources whose names are moved.to claims"
    );

    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &state_blocks,
        &Some(state_file),
    );
    assert_eq!(moved_pairs.len(), 5);
    for (idx, (from, to)) in moved_pairs.iter().enumerate() {
        assert_eq!(from.name_str(), old_names[idx]);
        assert_eq!(to.name_str(), desired_names[idx]);
    }
    for name in old_names {
        let id = ResourceId::with_provider("awscc", "ec2.SubnetRouteTableAssociation", name, None);
        assert!(
            !current_states.contains_key(&id),
            "old state key must be removed after materialized move"
        );
    }
    for name in desired_names {
        let id = ResourceId::with_provider("awscc", "ec2.SubnetRouteTableAssociation", name, None);
        assert!(
            current_states.contains_key(&id),
            "desired state key must exist after materialized move"
        );
    }
}

#[tokio::test]
async fn moved_blocks_plan_level_fixture_emits_five_moves_only() {
    use carina_core::effect::Effect;
    use carina_core::parser::{ProviderContext, parse};
    use carina_state::state::{ResourceState, StateFile};

    // This plan-level fixture pins moved materialization and Move-effect
    // emission at the `create_plan_from_parsed_with_upstream` seam. The
    // claims-precedence ordering itself is covered by
    // `moved_blocks_are_honored_before_heuristic_reconciliation_for_five_renames`.
    let mut source = String::new();
    let mut state_file = StateFile::new();
    for idx in 0..5 {
        let old_name = format!("old_assoc_{idx}");
        let desired_name = format!("desired_assoc_{idx}");
        let route_table_id = format!("rtb-{idx}");
        let subnet_id = format!("subnet-{idx}");

        source.push_str(&format!(
            r#"
let {desired_name} = awscc.ec2.SubnetRouteTableAssociation {{
    route_table_id = "{route_table_id}"
    subnet_id      = "{subnet_id}"
}}

moved {{
    from = awscc.ec2.SubnetRouteTableAssociation "{old_name}"
    to   = awscc.ec2.SubnetRouteTableAssociation "{desired_name}"
}}
"#
        ));

        state_file.resources.push(
            ResourceState::new("ec2.SubnetRouteTableAssociation", &old_name, "awscc")
                .with_identifier(format!("assoc-{idx}"))
                .with_attribute("route_table_id", serde_json::Value::String(route_table_id))
                .with_attribute("subnet_id", serde_json::Value::String(subnet_id)),
        );
    }

    let parsed = parse(&source, &ProviderContext::default()).expect("parse fixture");
    let StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = resolve_state_blocks(
        &parsed.state_blocks,
        &Some(state_file.clone()),
        &parsed.resources,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = create_plan_from_parsed_with_upstream(
        &parsed,
        &Some(state_file),
        false,
        &HashMap::new(),
        &state_block_claims,
        &resolved_state_block_targets,
        tmp.path(),
    )
    .await
    .expect("plan fixture");

    let summary = ctx.plan.summary();
    assert_eq!(summary.create, 0, "moved fixture must not add resources");
    assert_eq!(
        summary.delete, 0,
        "moved fixture must not destroy resources"
    );
    assert_eq!(
        summary.replace, 0,
        "moved fixture must not replace resources"
    );
    assert_eq!(summary.update, 0, "moved fixture must not update resources");
    assert_eq!(summary.moved, 5, "all five moved blocks must be honored");
    assert_eq!(ctx.plan.mutation_count(), 5);
    assert!(
        ctx.plan
            .effects()
            .iter()
            .all(|effect| matches!(effect, Effect::Move { .. })),
        "plan must contain only Move effects, got {:?}",
        ctx.plan.effects()
    );
}

/// Regression test for carina#1683: data source input attributes that
/// reference another resource must be resolved against current state
/// *before* being passed to `read_data_source_with_retry`. Without
/// resolution the provider receives a debug-formatted `ResourceRef`
/// string and ships it to the remote API as a literal.
#[test]
fn resolve_data_source_refs_replaces_resource_ref_with_concrete_value() {
    use carina_core::resource::{AccessPath, DataSource};

    let identity_store_id = "d-9067c29a4b";

    // Managed resource with a binding — phase 1 would have refreshed it.
    let mut sso = Resource::with_provider("awscc", "sso.Instance", "carina-rs", None);
    sso.binding = Some("sso".to_string());

    // Data source referencing `sso.identity_store_id`. carina#3181:
    // data sources are a distinct typestate.
    let mut mizzy = DataSource::with_provider("aws", "identitystore.user", "mizzy", None);
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
        &[sso],
        &[],
        &[mizzy],
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

    let canonical_resources =
        carina_core::value::canonicalize_resources_with_schemas(&mut resources, ctx.schemas());
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, canonical_resources, &providers);
    assert!(
        errors.is_empty(),
        "compute_anonymous_identifiers: got {errors:?}",
    );
}

struct RegionIdentityFactory;

impl carina_core::provider::ProviderFactory for RegionIdentityFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC region identity test provider"
    }

    fn provider_config_attribute_types(
        &self,
    ) -> HashMap<String, carina_core::schema::AttributeType> {
        HashMap::from([(
            "region".to_string(),
            carina_core::schema::AttributeType::enum_(
                carina_core::schema::TypeIdentity::new(
                    Some("awscc"),
                    Vec::<String>::new(),
                    "Region",
                ),
                None,
                Vec::new(),
                None,
                Some(carina_core::schema::DslTransform::HyphenToUnderscore),
            ),
        )])
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<Box<dyn carina_core::provider::Provider>>,
    > {
        Box::pin(async {
            Ok(Box::new(MockProvider::new()) as Box<dyn carina_core::provider::Provider>)
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> carina_core::provider::BoxFuture<'_, Box<dyn carina_core::provider::ProviderNormalizer>>
    {
        Box::pin(async {
            Box::new(carina_core::provider::NoopNormalizer)
                as Box<dyn carina_core::provider::ProviderNormalizer>
        })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
        vec![
            carina_core::schema::ResourceSchema::new("ec2.Route").attribute(
                carina_core::schema::AttributeSchema::new(
                    "route_table_id",
                    carina_core::schema::AttributeType::string(),
                ),
            ),
        ]
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }
}

fn region_identity_ctx() -> WiringContext {
    WiringContext::new(vec![Box::new(RegionIdentityFactory)])
}

fn region_provider_config(raw_region: &str) -> ProviderConfig {
    ProviderConfig {
        name: "awscc".to_string(),
        attributes: indexmap::indexmap! {
            "region".to_string() => Value::Concrete(ConcreteValue::enum_identifier(raw_region)),
        },
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
    }
}

fn anonymous_route_resource() -> Resource {
    let mut resource = Resource::with_provider("awscc", "ec2.Route", "", None);
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Concrete(ConcreteValue::String("rtb-123".to_string())),
    );
    resource
}

#[test]
fn compute_anonymous_identifiers_with_ctx_canonicalizes_provider_config_identity_enums() {
    let ctx = region_identity_ctx();
    let providers_awscc = vec![region_provider_config("awscc.Region.ap_northeast_1")];
    let providers_aws = vec![region_provider_config("aws.Region.ap_northeast_1")];

    let mut resources_awscc = vec![anonymous_route_resource()];
    let mut resources_aws = vec![anonymous_route_resource()];
    let canonical_awscc = carina_core::value::canonicalize_resources_with_schemas(
        &mut resources_awscc,
        ctx.schemas(),
    );
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, canonical_awscc, &providers_awscc);
    assert!(errors.is_empty(), "awscc spelling errors: {errors:?}");
    let canonical_aws =
        carina_core::value::canonicalize_resources_with_schemas(&mut resources_aws, ctx.schemas());
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, canonical_aws, &providers_aws);
    assert!(errors.is_empty(), "aws spelling errors: {errors:?}");

    assert_eq!(
        resources_awscc[0].id.name_str(),
        resources_aws[0].id.name_str(),
        "provider config region spelling must canonicalize before anonymous hash"
    );
}

#[test]
fn apply_anonymous_to_named_renames_canonicalizes_provider_config_identity_enums() {
    use carina_state::state::{ResourceState, StateFile};

    let ctx = region_identity_ctx();
    let providers_awscc = vec![region_provider_config("awscc.Region.ap_northeast_1")];
    let providers_aws = vec![region_provider_config("aws.Region.ap_northeast_1")];

    let mut anonymous = vec![anonymous_route_resource()];
    let canonical_anonymous =
        carina_core::value::canonicalize_resources_with_schemas(&mut anonymous, ctx.schemas());
    let errors =
        compute_anonymous_identifiers_with_ctx(&ctx, canonical_anonymous, &providers_awscc);
    assert!(errors.is_empty(), "anonymous setup errors: {errors:?}");
    let old_name = anonymous[0].id.name_str().to_string();

    let mut named = anonymous_route_resource();
    named.id = ResourceId::with_provider("awscc", "ec2.Route", "route", None);
    named.binding = Some("route".to_string());
    let resources = vec![named.clone()];

    let mut state_file = StateFile::new();
    state_file
        .resources
        .push(ResourceState::new("ec2.Route", &old_name, "awscc"));
    let mut current_states = HashMap::new();
    let mut prev_explicit = HashMap::new();
    let mut saved_attrs = HashMap::new();

    let renames = apply_anonymous_to_named_renames(
        &ctx,
        &resources,
        &providers_aws,
        &mut current_states,
        &mut prev_explicit,
        &mut saved_attrs,
        &Some(state_file),
        &carina_core::identifier::StateBlockClaims::empty(),
    );

    assert_eq!(
        renames,
        vec![(
            ResourceId::with_provider("awscc", "ec2.Route", old_name, None),
            named.id
        )],
        "provider config region spelling must canonicalize before rename simhash"
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
    use carina_core::resource::{DataSource, State};
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

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
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

    /// Direct unit test of `RefreshableChildIds::select` — the sole
    /// constructor of the post-expansion refresh iterator. The
    /// `expand_*` tests cover it end-to-end, but `select` carries the
    /// carina#3141 invariant ("yield exactly the ids in the set, skip
    /// everything else") and a refactor could break it without tripping
    /// the higher-level tests, so pin it directly. Builds a
    /// `RefreshableChildIds` from a known id subset and asserts `select`
    /// over a resource slice yields precisely those resources, in slice
    /// order, and nothing outside the set.
    #[test]
    fn refreshable_child_ids_select_yields_exactly_the_set() {
        // `Resource::new` gives each resource a distinct, concrete
        // `ResourceId` directly — no parse/ID-reconcile step, so the
        // test pins `select`'s set-membership logic in isolation
        // (anonymous ids are still pending right after `parse`, which is
        // a different concern covered by the expand_* tests).
        let r_a = Resource::new("aws.ec2.Vpc", "a");
        let r_b = Resource::new("aws.ec2.Vpc", "b");
        let r_c = Resource::new("aws.ec2.Vpc", "c");
        let resources = vec![r_a.clone(), r_b.clone(), r_c.clone()];

        // Refreshable set = {a, c}; b must be skipped.
        let mut ids = std::collections::HashSet::new();
        ids.insert(r_a.id.clone());
        ids.insert(r_c.id.clone());
        let refreshable = RefreshableChildIds(ids);

        assert_eq!(refreshable.len(), 2);
        assert!(!refreshable.is_empty());
        assert!(refreshable.contains(&r_a.id));
        assert!(!refreshable.contains(&r_b.id));

        let selected: Vec<&ResourceId> = refreshable.select(&resources).map(|r| &r.id).collect();
        assert_eq!(
            selected,
            vec![&r_a.id, &r_c.id],
            "select must yield exactly the set members, in slice order, \
             and skip the resource not in the set"
        );

        // An empty set selects nothing — the "all expanded children are
        // moved targets" case from the plan/apply guard.
        let empty = RefreshableChildIds::default();
        assert!(empty.is_empty());
        assert_eq!(empty.select(&resources).count(), 0);
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
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
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
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
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
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
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
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
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
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
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

    /// The carina#3141 repro DSL: `let cert` + same-config
    /// `for (_, opt) in cert.domain_validation_options` loop. Builds the
    /// expansion once with NO moved targets, then again declaring the
    /// materialized child as a `moved` `to`. Asserts:
    ///   - `new_child_ids` is identical in both runs (the moved-exclusion
    ///     must not change what materialized).
    ///   - run 1: the child IS in `refreshable_child_ids` (non-moved
    ///     expanded children are still refreshed — regression guard
    ///     against an over-broad filter).
    ///   - run 2: the child is NOT in `refreshable_child_ids` (its
    ///     migrated state from `materialize_moved_states` must survive,
    ///     so the differ emits `Move`, never a same-address `Create`).
    ///
    /// Both the plan path (`create_plan_from_parsed_with_upstream`) and
    /// the apply path (`run_apply_locked`) refresh exactly
    /// `refreshable_child_ids` — this single typed field is what makes
    /// the moved-exclusion impossible to diverge between them
    /// (cf. MEMORY "unit-test path ≠ apply path"; the divergence is a
    /// compile error against `DeferredForExpansion`, not a runtime gap).
    #[test]
    fn moved_target_child_is_excluded_from_refreshable_set() {
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

        // Run 1: no moved targets — the expanded child must be both a
        // new child AND refreshable (regression guard: the #3141 filter
        // must not skip refresh for ordinary expanded children).
        let out_no_moved = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand without moved targets");

        assert_eq!(
            out_no_moved.new_child_ids.len(),
            1,
            "one RecordSet materialized per domain_validation_options entry"
        );
        let child_id = out_no_moved
            .new_child_ids
            .iter()
            .next()
            .expect("the materialized child id")
            .clone();
        assert!(
            out_no_moved.refreshable_child_ids.contains(&child_id),
            "a non-moved expanded child must still be refreshed"
        );

        // Run 2: declare that same child id as a `moved` `to`.
        let mut moved_targets = std::collections::HashSet::new();
        moved_targets.insert(child_id.clone());
        let out_moved = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &moved_targets,
            &std::collections::HashSet::new(),
        )
        .expect("expand with the child as a moved target");

        assert_eq!(
            out_moved.new_child_ids, out_no_moved.new_child_ids,
            "the moved-exclusion must not change what materialized"
        );
        assert!(
            !out_moved.refreshable_child_ids.contains(&child_id),
            "a child that is a `moved` target must be excluded from \
             refreshable_child_ids so its migrated state is not \
             overwritten by a not_found provider read (carina#3141)"
        );
        assert!(
            out_moved.refreshable_child_ids.is_empty(),
            "the only expanded child is the moved target, so nothing \
             is refreshable; got {:?}",
            out_moved.refreshable_child_ids
        );
    }

    /// Multi-entry variant: ≥2 `domain_validation_options` (real ACM
    /// certs have apex + SAN). Moving only the first index must exclude
    /// exactly that child from refresh and leave the second one
    /// refreshable — the per-index `moved` (`[0]`, `[1]`, …) and the
    /// exclusion set are both exercised, not just the single-entry case.
    #[test]
    fn moved_exclusion_is_per_child_with_multiple_entries() {
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

        let make_entry = |name: &str, value: &str| {
            let mut rr = indexmap::IndexMap::new();
            rr.insert(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String(name.into())),
            );
            rr.insert(
                "value".to_string(),
                Value::Concrete(ConcreteValue::String(value.into())),
            );
            let mut entry = indexmap::IndexMap::new();
            entry.insert(
                "resource_record".to_string(),
                Value::Concrete(ConcreteValue::Map(rr)),
            );
            Value::Concrete(ConcreteValue::Map(entry))
        };
        let dvo = Value::Concrete(ConcreteValue::List(vec![
            make_entry("_apex.r.example.com", "_apex.acm-validations.aws."),
            make_entry("_san.r.example.com", "_san.acm-validations.aws."),
        ]));
        let states = states_with_cert(&parsed, "domain_validation_options", dvo);

        let baseline = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");
        assert_eq!(
            baseline.new_child_ids.len(),
            2,
            "two RecordSets materialized (apex + SAN)"
        );

        // Move only one of the two children.
        let mut ids: Vec<ResourceId> = baseline.new_child_ids.iter().cloned().collect();
        ids.sort_by_key(|id| id.to_string());
        let moved_child = ids[0].clone();
        let kept_child = ids[1].clone();

        let mut moved_targets = std::collections::HashSet::new();
        moved_targets.insert(moved_child.clone());
        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &moved_targets,
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert_eq!(
            out.new_child_ids, baseline.new_child_ids,
            "moved-exclusion must not change what materialized"
        );
        assert!(
            !out.refreshable_child_ids.contains(&moved_child),
            "the moved child must be excluded from refresh"
        );
        assert!(
            out.refreshable_child_ids.contains(&kept_child),
            "the non-moved child must still be refreshed"
        );
        assert_eq!(
            out.refreshable_child_ids.len(),
            1,
            "exactly one of two children is refreshable; got {:?}",
            out.refreshable_child_ids
        );
    }

    /// carina#3145: a for-loop child that was applied on a previous run
    /// is in the state file. On the next `plan`/`apply`, the phase-1
    /// orphan pass classifies it as an orphan (it is not yet a desired
    /// resource — expansion happens *after* refresh) and performs a live
    /// provider read of it, storing the result in `current_states`. The
    /// post-expansion child refresh then reads the *same* address a
    /// second time. Two live provider reads for one resource.
    ///
    /// The fix decides "already live-read this run" *once*, in the same
    /// place the moved-target exclusion is decided, and carries it in the
    /// typed `RefreshableChildIds` so the plan and apply paths cannot
    /// diverge. This test pins that contract: an id in `already_refreshed`
    /// must materialize as a child but be excluded from
    /// `refreshable_child_ids` (no redundant second read), while a child
    /// not in that set stays refreshable.
    #[test]
    fn already_refreshed_child_is_excluded_from_refreshable_set() {
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

        let make_entry = |name: &str, value: &str| {
            let mut rr = indexmap::IndexMap::new();
            rr.insert(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String(name.into())),
            );
            rr.insert(
                "value".to_string(),
                Value::Concrete(ConcreteValue::String(value.into())),
            );
            let mut entry = indexmap::IndexMap::new();
            entry.insert(
                "resource_record".to_string(),
                Value::Concrete(ConcreteValue::Map(rr)),
            );
            Value::Concrete(ConcreteValue::Map(entry))
        };
        let dvo = Value::Concrete(ConcreteValue::List(vec![
            make_entry("_apex.r.example.com", "_apex.acm-validations.aws."),
            make_entry("_san.r.example.com", "_san.acm-validations.aws."),
        ]));
        let states = states_with_cert(&parsed, "domain_validation_options", dvo);

        // Baseline: nothing pre-refreshed — both children materialize and
        // are refreshable (the single live read happens post-expansion).
        let baseline = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");
        assert_eq!(
            baseline.new_child_ids.len(),
            2,
            "two RecordSets materialized (apex + SAN)"
        );
        assert_eq!(
            baseline.refreshable_child_ids.len(),
            2,
            "with nothing pre-refreshed both children are refreshable"
        );

        // The phase-1 orphan pass already live-read the first child this
        // run (it is in the state file from a prior apply, not yet a
        // desired resource at orphan time).
        let mut ids: Vec<ResourceId> = baseline.new_child_ids.iter().cloned().collect();
        ids.sort_by_key(|id| id.to_string());
        let already_read = ids[0].clone();
        let still_unread = ids[1].clone();

        let mut already_refreshed = std::collections::HashSet::new();
        already_refreshed.insert(already_read.clone());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &states,
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &already_refreshed,
        )
        .expect("expand");

        assert_eq!(
            out.new_child_ids, baseline.new_child_ids,
            "the already-refreshed exclusion must not change what materialized"
        );
        assert!(
            !out.refreshable_child_ids.contains(&already_read),
            "a child already live-read by the phase-1 orphan pass must be \
             excluded from refreshable_child_ids so it is not read a \
             second time (carina#3145)"
        );
        assert!(
            out.refreshable_child_ids.contains(&still_unread),
            "a child not yet read this run must still be refreshed"
        );
        assert_eq!(
            out.refreshable_child_ids.len(),
            1,
            "exactly one of two children is refreshable; got {:?}",
            out.refreshable_child_ids
        );
    }
}

/// Tests for the unified "Using <provider>" announcement format
/// (carina#3067). The default-instance and named-instance code paths
/// must emit the same shape so that CI logs are unambiguous about
/// which kind / instance / region was wired up.
#[cfg(test)]
mod using_line_format_tests {
    use super::super::format_provider_using_line;

    #[test]
    fn default_instance_with_source() {
        // Default instance (no binding). Source is always known on this
        // path because the default instance is the only one that
        // load-sources a factory.
        assert_eq!(
            format_provider_using_line(
                "awscc",
                "ap-northeast-1",
                None,
                Some("github.com/carina-rs/carina-provider-awscc"),
            ),
            "Using awscc (region: ap-northeast-1, instance=default, \
             source: github.com/carina-rs/carina-provider-awscc)",
        );
    }

    #[test]
    fn named_instance_with_source() {
        // Named instance. Source belongs to the kind, not the instance,
        // and is still included so that one log line is enough to
        // identify what got wired up.
        assert_eq!(
            format_provider_using_line(
                "aws",
                "us-east-1",
                Some("us"),
                Some("github.com/carina-rs/carina-provider-aws"),
            ),
            "Using aws (region: us-east-1, instance=us, \
             source: github.com/carina-rs/carina-provider-aws)",
        );
    }

    #[test]
    fn instance_label_uses_default_marker_when_unbound() {
        // The kind's default instance must be labeled "instance=default"
        // explicitly, not omitted — the omission is what made the old
        // default-line and named-line shapes diverge.
        let line = format_provider_using_line("aws", "ap-northeast-1", None, None);
        assert!(
            line.contains("instance=default"),
            "default instance must surface as 'instance=default'; got: {line}",
        );
    }

    #[test]
    fn kind_label_is_kind_name_not_display_name() {
        // The label must be the kind name ("aws") — what the user
        // writes in `provider <kind> {}` and routes to via
        // `directives.provider` — not the factory's display_name
        // ("AWS provider"). The old named-instance path used
        // display_name, which made the two log lines disagree on
        // what to call the same construct.
        let line = format_provider_using_line("aws", "us-east-1", Some("us"), None);
        assert!(
            line.starts_with("Using aws "),
            "kind label must be the kind name, not a display name; got: {line}",
        );
    }
}

#[cfg(test)]
mod kind_default_source_tests {
    use super::super::kind_default_source;
    use carina_core::parser::ProviderConfig;
    use indexmap::IndexMap;

    fn config(name: &str, is_default: bool, source: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: source.map(str::to_string),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: if is_default {
                None
            } else {
                Some("named".to_string())
            },
            is_default,
        }
    }

    #[test]
    fn returns_source_of_matching_kind_default() {
        let configs = vec![
            config(
                "aws",
                true,
                Some("github.com/carina-rs/carina-provider-aws"),
            ),
            config("aws", false, None),
        ];
        assert_eq!(
            kind_default_source(&configs, "aws"),
            Some("github.com/carina-rs/carina-provider-aws"),
        );
    }

    #[test]
    fn ignores_named_instances_with_same_kind() {
        // Named instance comes first in the slice. The function must
        // still pick the default, not the named — otherwise it would
        // return the named instance's (always-None) source.
        let configs = vec![
            config("aws", false, None),
            config(
                "aws",
                true,
                Some("github.com/carina-rs/carina-provider-aws"),
            ),
        ];
        assert_eq!(
            kind_default_source(&configs, "aws"),
            Some("github.com/carina-rs/carina-provider-aws"),
        );
    }

    #[test]
    fn different_kinds_do_not_cross_leak() {
        // Two kinds, each with their own default + source. Asking for
        // one kind must never return the other's source.
        let configs = vec![
            config(
                "aws",
                true,
                Some("github.com/carina-rs/carina-provider-aws"),
            ),
            config(
                "awscc",
                true,
                Some("github.com/carina-rs/carina-provider-awscc"),
            ),
        ];
        assert_eq!(
            kind_default_source(&configs, "aws"),
            Some("github.com/carina-rs/carina-provider-aws"),
        );
        assert_eq!(
            kind_default_source(&configs, "awscc"),
            Some("github.com/carina-rs/carina-provider-awscc"),
        );
    }

    #[test]
    fn returns_none_when_kind_default_has_no_source() {
        let configs = vec![config("mock", true, None)];
        assert_eq!(kind_default_source(&configs, "mock"), None);
    }

    #[test]
    fn returns_none_when_kind_is_absent() {
        let configs = vec![config(
            "aws",
            true,
            Some("github.com/carina-rs/carina-provider-aws"),
        )];
        assert_eq!(kind_default_source(&configs, "awscc"), None);
    }
}

/// carina#3358 (reopened after PR #3359): an enum-form `until` predicate
/// must be resolved to the canonical AWS value before the differ lowers
/// the wait into `Effect::Wait`, so an already-satisfied wait is elided.
///
/// PR #3359's gate calls `until.evaluate(&state.attributes)`. The parser
/// lowers `until = cert.status == aws.acm.Certificate.Status.Issued` into
/// the RAW dotted string `"aws.acm.Certificate.Status.Issued"`; the
/// canonical state value is `"ISSUED"`. Unless the alias is resolved the
/// two never compare equal, so the gate's `target_needs_wait` stays
/// `true` and the no-op wait is emitted — the exact registry-dev repro.
mod wait_until_enum_alias {
    use super::super::*;
    use carina_core::parser::{UntilPredicateAst, WaitBinding};
    use carina_core::provider::{
        BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
    };
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, enum_identity};

    /// Minimal aws-like factory that mirrors the REAL
    /// `carina-provider-aws` ACM `status` enum reverse alias:
    /// `("status", "issued") => "ISSUED"` (snake_case DSL spelling →
    /// canonical AWS value; verified against
    /// `carina-provider-aws/.../acm/certificate.rs`). The DSL value
    /// `aws.acm.Certificate.Status.issued` — the exact form in the
    /// reopened issue #3358 — is shortened by
    /// `resolve_value_alias_with_schemas` to the trailing segment `"issued"`,
    /// which is the alias-map key. Keying on
    /// PascalCase `"Issued"` would NOT match the real provider, so the
    /// test would prove a path production never exercises. Everything
    /// else is stubbed to the minimum the helper needs: an enum schema for
    /// `status` and `get_enum_alias_reverse`.
    struct AcmAliasFactory;

    impl ProviderFactory for AcmAliasFactory {
        fn name(&self) -> &str {
            "aws"
        }
        fn display_name(&self) -> &str {
            "AWS (carina#3358 enum-alias stub)"
        }
        fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
            HashMap::new()
        }
        fn validate_config(&self, _a: &IndexMap<String, Value>) -> Result<(), String> {
            Ok(())
        }
        fn extract_region(&self, _a: &IndexMap<String, Value>) -> String {
            "us-east-1".to_string()
        }
        fn create_provider(
            &self,
            _b: Option<&str>,
            _a: &IndexMap<String, Value>,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async { Ok(Box::new(StubProvider) as Box<dyn Provider>) })
        }
        fn create_normalizer(
            &self,
            _b: Option<&str>,
            _a: &IndexMap<String, Value>,
        ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
            Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
        }
        fn schemas(&self) -> Vec<ResourceSchema> {
            vec![
                ResourceSchema::new("acm.Certificate").attribute(AttributeSchema::new(
                    "status",
                    AttributeType::enum_(
                        enum_identity("Status", Some("aws.acm.Certificate")),
                        Some(vec!["issued".to_string(), "pending_validation".to_string()]),
                        Vec::new(),
                        None,
                        None,
                    ),
                )),
            ]
        }
        fn get_enum_alias_reverse(
            &self,
            resource_type: &str,
            attr_name: &str,
            value: &str,
        ) -> Option<String> {
            match (resource_type, attr_name, value) {
                ("acm.Certificate", "status", "issued") => Some("ISSUED".to_string()),
                _ => None,
            }
        }
    }

    // The provider is never instantiated. These methods exist only to satisfy
    // the trait and are unreachable in these tests.
    struct StubProvider;
    impl Provider for StubProvider {
        fn name(&self) -> &str {
            "aws"
        }
        fn read(
            &self,
            id: &ResourceId,
            _i: Option<&str>,
            _r: carina_core::provider::ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }
        fn read_data_source(
            &self,
            r: &carina_core::resource::DataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = r.id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }
        fn create(
            &self,
            id: &ResourceId,
            _r: carina_core::provider::CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }
        fn update(
            &self,
            id: &ResourceId,
            _i: &str,
            _r: carina_core::provider::UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }
        fn delete(
            &self,
            _id: &ResourceId,
            _i: &str,
            _r: carina_core::provider::DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    fn enum_wait_binding() -> WaitBinding {
        WaitBinding {
            binding: "cert_issued".into(),
            target: "cert".into(),
            // The exact DSL form from the reopened issue #3358
            // (snake_case `issued`, matching the real provider alias).
            until_raw: "cert.status == aws.acm.Certificate.Status.issued".to_string(),
            until_predicate: UntilPredicateAst {
                lhs_segments: vec!["cert".to_string(), "status".to_string()],
                // Exactly what `lower_until_rhs` produces for the enum form:
                // the raw dotted identifier, NOT the canonical value.
                rhs: Value::Concrete(ConcreteValue::String(
                    "aws.acm.Certificate.Status.issued".to_string(),
                )),
            },
            timeout_secs: Some(60),
            depends_on: vec![],
            line: 1,
        }
    }

    fn cert_resource() -> Resource {
        // Unchanged cert: desired matches state (status already ISSUED).
        let mut r = Resource::with_provider("aws", "acm.Certificate", "cert", None);
        r.binding = Some("cert".to_string());
        r.set_attr(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        );
        r
    }

    fn changed_consumer() -> Resource {
        // New (absent from state) → Create → mutating; references the
        // wait binding so `gates_a_pending_change` is satisfied.
        let mut r = Resource::with_provider("aws", "cloudfront.Distribution", "dist", None);
        r.binding = Some("dist".to_string());
        r.dependency_bindings.insert("cert_issued".to_string());
        r
    }

    fn cert_state() -> HashMap<ResourceId, State> {
        let id = ResourceId::with_provider("aws", "acm.Certificate", "cert", None);
        let mut attrs = HashMap::new();
        attrs.insert(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        );
        let mut states = HashMap::new();
        states.insert(
            id.clone(),
            State::existing(id, attrs).with_identifier("arn:aws:acm:::certificate/abc"),
        );
        states
    }

    /// The fix's core: `resolve_enum_aliases_in_wait_bindings` rewrites
    /// the raw enum RHS to the canonical AWS value using the target
    /// resource's `(provider, resource_type)` and the factory alias map.
    #[test]
    fn helper_resolves_until_rhs_enum_alias_to_canonical() {
        let ctx = WiringContext::new(vec![Box::new(AcmAliasFactory) as Box<dyn ProviderFactory>]);
        let mut waits = vec![enum_wait_binding()];
        let resources = vec![cert_resource()];

        resolve_enum_aliases_in_wait_bindings(&ctx, &mut waits, &resources, &[]);

        assert_eq!(
            waits[0].until_predicate.rhs,
            Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            "until RHS enum identifier must resolve to the canonical AWS value"
        );
    }

    /// End-to-end logic at the plan seam: with the RAW enum RHS the
    /// differ emits a no-op wait (the bug); after the wiring's
    /// enum-alias resolution it is elided. Drives the real
    /// `resolve_enum_aliases_in_wait_bindings` -> `create_plan` pair the
    /// plan path uses.
    #[test]
    fn create_plan_elides_already_satisfied_wait_after_enum_alias_resolution() {
        let ctx = WiringContext::new(vec![Box::new(AcmAliasFactory) as Box<dyn ProviderFactory>]);
        let resources = vec![cert_resource(), changed_consumer()];
        let states = cert_state();

        // Bug repro: unresolved RAW enum RHS → wait wrongly emitted.
        let raw_waits = vec![enum_wait_binding()];
        let plan_raw = create_plan(
            &resources,
            &[],
            &states,
            &HashMap::new(),
            ctx.schemas(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &raw_waits,
        );
        assert!(
            plan_raw
                .effects()
                .iter()
                .any(|e| matches!(e, Effect::Wait { .. })),
            "precondition: with the raw enum RHS the differ emits the no-op \
             wait (the carina#3358 bug); effects were {:?}",
            plan_raw.effects()
        );

        // Fixed: resolve the alias first → predicate satisfied → elided.
        let mut waits = vec![enum_wait_binding()];
        resolve_enum_aliases_in_wait_bindings(&ctx, &mut waits, &resources, &[]);
        let plan_fixed = create_plan(
            &resources,
            &[],
            &states,
            &HashMap::new(),
            ctx.schemas(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &waits,
        );
        assert!(
            !plan_fixed
                .effects()
                .iter()
                .any(|e| matches!(e, Effect::Wait { .. })),
            "carina#3358: after enum-alias resolution the already-satisfied \
             wait must be elided; effects were {:?}",
            plan_fixed.effects()
        );
    }
}
