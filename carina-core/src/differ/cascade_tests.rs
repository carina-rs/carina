use super::*;

use crate::effect::ChangedCreateOnly;
use crate::resource::ConcreteValue;
use crate::schema::{AttributeSchema, ResourceSchema};

fn str_value(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string()))
}

fn ref_value(binding: &str, attr: &str) -> Value {
    Value::resource_ref(binding.to_string(), attr.to_string(), vec![])
}

fn state(id: ResourceId, attrs: impl IntoIterator<Item = (&'static str, Value)>) -> State {
    State::existing(
        id,
        attrs
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
    .with_identifier("existing-id")
}

fn base_registry() -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "",
        ResourceSchema::new("waf.WebAcl")
            .attribute(AttributeSchema::new("name", AttributeType::string()).create_only())
            .attribute(AttributeSchema::new("arn", AttributeType::string())),
    );
    registry.insert(
        "",
        ResourceSchema::new("cdn.Distribution")
            .attribute(AttributeSchema::new("web_acl_arn", AttributeType::string()))
            .attribute(AttributeSchema::new("comment", AttributeType::string()))
            .attribute(AttributeSchema::new("domain_name", AttributeType::string())),
    );
    registry
}

fn web_acl_id() -> ResourceId {
    ResourceId::new("waf.WebAcl", "main")
}

fn distribution_id() -> ResourceId {
    ResourceId::new("cdn.Distribution", "main")
}

fn seed_web_acl_replace(plan: &mut Plan, current_states: &HashMap<ResourceId, State>) {
    let id = web_acl_id();
    let from = current_states
        .get(&id)
        .unwrap_or_else(|| panic!("missing current state for {id}"));
    let mut to = web_acl();
    to.directives.create_before_destroy = true;
    let create_idx = plan.add(Effect::Create(to.clone()));
    let delete_idx = plan.add(Effect::Delete {
        id: id.clone(),
        identifier: from.identifier.clone().unwrap_or_default(),
        directives: to.directives.clone(),
        binding: Some("web_acl".to_string()),
        dependencies: from.dependency_bindings.iter().cloned().collect(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: std::collections::HashSet::new(),
    });
    plan.add_replace_display(crate::plan::ReplaceDisplayMetadata {
        id,
        binding: Some("web_acl".to_string()),
        create_idx,
        delete_idx,
        create_before_destroy: true,
        changed_create_only: ChangedCreateOnly::new(vec!["name".to_string()]).unwrap(),
        cascade_ref_hints: vec![],
        temporary_name: None,
        previous_attributes: from.attributes.clone(),
    });
}

fn cascade_plan_for(
    resources: &[Resource],
    current_states: HashMap<ResourceId, State>,
    registry: &SchemaRegistry,
) -> Plan {
    let mut plan = Plan::new();
    seed_web_acl_replace(&mut plan, &current_states);
    cascade_dependent_updates(
        &mut plan,
        resources,
        &crate::resource::into_plan_input_map(current_states),
        registry,
    );
    plan
}

fn web_acl() -> Resource {
    Resource::new("waf.WebAcl", "main")
        .with_binding("web_acl")
        .with_attribute("name", str_value("new-acl"))
        .with_attribute("arn", str_value("arn:new"))
}

fn distribution() -> Resource {
    Resource::new("cdn.Distribution", "main")
        .with_binding("distribution")
        .with_attribute("web_acl_arn", ref_value("web_acl", "arn"))
        .with_attribute("comment", str_value("unchanged"))
        .with_attribute("domain_name", str_value("example.cloudfront.net"))
}

fn current_web_acl_state() -> State {
    state(
        web_acl_id(),
        [
            ("name", str_value("old-acl")),
            ("arn", str_value("arn:old")),
        ],
    )
}

fn current_distribution_state(web_acl_value: Value, comment: &str) -> State {
    state(
        ResourceId::new("cdn.Distribution", "main"),
        [
            ("web_acl_arn", web_acl_value),
            ("comment", str_value(comment)),
            ("domain_name", str_value("example.cloudfront.net")),
        ],
    )
}

fn delete_blockers_for<'a>(
    plan: &'a Plan,
    id: &ResourceId,
) -> &'a std::collections::HashSet<String> {
    plan.effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::Delete {
                id: delete_id,
                blocked_by_updates,
                ..
            } if delete_id == id => Some(blocked_by_updates),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing Delete for {id}"))
}

fn updates_for<'a>(plan: &'a Plan, id: &ResourceId) -> Vec<&'a Effect> {
    plan.effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::Update { id: update_id, .. } if update_id == id))
        .collect()
}

fn replace_metadata_for<'a>(
    plan: &'a Plan,
    id: &ResourceId,
) -> &'a crate::plan::ReplaceDisplayMetadata {
    plan.replace_display()
        .iter()
        .find(|metadata| &metadata.id == id)
        .unwrap_or_else(|| panic!("missing replace display metadata for {id}"))
}

fn decomposed_replace_count(plan: &Plan, id: &ResourceId) -> (usize, usize) {
    let creates = plan
        .effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::Create(resource) if &resource.id == id))
        .count();
    let deletes = plan
        .effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::Delete { id: delete_id, .. } if delete_id == id))
        .count();
    (creates, deletes)
}

#[test]
fn cascade_dependent_updates_adds_independent_update_for_dependent() {
    let registry = base_registry();
    let web_acl_id = web_acl_id();
    let distribution_id = distribution_id();
    let current_states = HashMap::from([
        (web_acl_id.clone(), current_web_acl_state()),
        (
            distribution_id.clone(),
            current_distribution_state(ref_value("web_acl", "arn"), "unchanged"),
        ),
    ]);

    let plan = cascade_plan_for(&[web_acl(), distribution()], current_states, &registry);

    let updates = updates_for(&plan, &distribution_id);
    assert_eq!(updates.len(), 1, "expected one cascade Update");
    match updates[0] {
        Effect::Update {
            from,
            to,
            changed_attributes,
            ..
        } => {
            assert_eq!(
                from.attributes.get("web_acl_arn"),
                Some(&ref_value("web_acl", "arn"))
            );
            assert_eq!(
                to.get_attr("web_acl_arn"),
                Some(&ref_value("web_acl", "arn"))
            );
            assert_eq!(changed_attributes, &vec!["web_acl_arn".to_string()]);
        }
        other => panic!("expected Update, got {other:?}"),
    }
    assert!(
        delete_blockers_for(&plan, &web_acl_id).contains("distribution"),
        "web_acl delete must wait for distribution cascade update"
    );
}

#[test]
fn cascade_dependent_updates_does_not_duplicate_when_existing_update() {
    let registry = base_registry();
    let web_acl_id = web_acl_id();
    let distribution_id = distribution_id();
    let current_states = HashMap::from([
        (web_acl_id.clone(), current_web_acl_state()),
        (
            distribution_id.clone(),
            current_distribution_state(ref_value("web_acl", "arn"), "old-comment"),
        ),
    ]);

    let mut plan = Plan::new();
    seed_web_acl_replace(&mut plan, &current_states);
    plan.add(Effect::Update {
        id: distribution_id.clone(),
        from: Box::new(current_states.get(&distribution_id).unwrap().clone()),
        to: distribution(),
        changed_attributes: vec!["comment".to_string()],
    });
    cascade_dependent_updates(
        &mut plan,
        &[web_acl(), distribution()],
        &crate::resource::into_plan_input_map(current_states),
        &registry,
    );

    let updates = updates_for(&plan, &distribution_id);
    assert_eq!(updates.len(), 1, "existing Update should be reused");
    match updates[0] {
        Effect::Update {
            changed_attributes, ..
        } => {
            assert_eq!(changed_attributes, &vec!["comment".to_string()]);
        }
        other => panic!("expected Update, got {other:?}"),
    }
    assert!(
        delete_blockers_for(&plan, &web_acl_id).contains("distribution"),
        "web_acl delete must wait for the existing distribution update"
    );
}

#[test]
fn cascade_dependent_updates_promotes_create_only_dependent_to_replace() {
    let mut registry = base_registry();
    registry.insert(
        "",
        ResourceSchema::new("cdn.Distribution")
            .attribute(AttributeSchema::new("web_acl_arn", AttributeType::string()).create_only())
            .attribute(AttributeSchema::new("comment", AttributeType::string()))
            .attribute(AttributeSchema::new("domain_name", AttributeType::string())),
    );
    let web_acl_id = web_acl_id();
    let distribution_id = distribution_id();
    let current_states = HashMap::from([
        (web_acl_id.clone(), current_web_acl_state()),
        (
            distribution_id.clone(),
            current_distribution_state(ref_value("web_acl", "arn"), "unchanged"),
        ),
    ]);

    let plan = cascade_plan_for(&[web_acl(), distribution()], current_states, &registry);

    assert_eq!(updates_for(&plan, &distribution_id).len(), 0);
    assert_eq!(decomposed_replace_count(&plan, &distribution_id), (1, 1));
    let metadata = replace_metadata_for(&plan, &distribution_id);
    assert!(metadata.changed_create_only.contains("web_acl_arn"));
    assert!(
        metadata
            .cascade_ref_hints
            .contains(&("web_acl_arn".to_string(), "web_acl.arn".to_string()))
    );
}

#[test]
fn cascade_dependent_updates_transitive() {
    let registry = base_registry();
    let web_acl_id = web_acl_id();
    let distribution_id = distribution_id();
    let cache_id = ResourceId::new("cdn.CachePolicy", "main");
    let cache = Resource::new("cdn.CachePolicy", "main")
        .with_binding("cache_policy")
        .with_attribute(
            "distribution_domain",
            ref_value("distribution", "domain_name"),
        );
    let current_states = HashMap::from([
        (web_acl_id.clone(), current_web_acl_state()),
        (
            distribution_id.clone(),
            current_distribution_state(ref_value("web_acl", "arn"), "unchanged"),
        ),
        (
            cache_id.clone(),
            state(
                cache_id.clone(),
                [(
                    "distribution_domain",
                    ref_value("distribution", "domain_name"),
                )],
            ),
        ),
    ]);

    let plan = cascade_plan_for(
        &[web_acl(), distribution(), cache],
        current_states,
        &registry,
    );

    assert_eq!(updates_for(&plan, &distribution_id).len(), 1);
    assert_eq!(
        updates_for(&plan, &cache_id).len(),
        0,
        "updating B for A replacement must not cascade to C when B is not replaced"
    );
    assert!(delete_blockers_for(&plan, &web_acl_id).contains("distribution"));
}

#[test]
fn chained_cbd_cascade_promoted_consumer_also_cbd() {
    let mut registry = base_registry();
    registry.insert(
        "",
        ResourceSchema::new("cdn.Distribution")
            .attribute(AttributeSchema::new("web_acl_arn", AttributeType::string()).create_only())
            .attribute(AttributeSchema::new("comment", AttributeType::string()))
            .attribute(AttributeSchema::new("domain_name", AttributeType::string())),
    );
    registry.insert(
        "",
        ResourceSchema::new("cdn.CachePolicy")
            .attribute(AttributeSchema::new(
                "distribution_domain",
                AttributeType::string(),
            ))
            .attribute(AttributeSchema::new("comment", AttributeType::string())),
    );
    let web_acl_id = web_acl_id();
    let distribution_id = distribution_id();
    let cache_id = ResourceId::new("cdn.CachePolicy", "main");
    let cache = Resource::new("cdn.CachePolicy", "main")
        .with_binding("cache_policy")
        .with_attribute(
            "distribution_domain",
            ref_value("distribution", "domain_name"),
        )
        .with_attribute("comment", str_value("unchanged"));
    let mut distribution_state =
        current_distribution_state(ref_value("web_acl", "arn"), "unchanged");
    distribution_state
        .dependency_bindings
        .insert("web_acl".to_string());
    let mut cache_state = state(
        cache_id.clone(),
        [
            (
                "distribution_domain",
                ref_value("distribution", "domain_name"),
            ),
            ("comment", str_value("unchanged")),
        ],
    );
    cache_state
        .dependency_bindings
        .insert("distribution".to_string());
    let current_states = HashMap::from([
        (web_acl_id.clone(), current_web_acl_state()),
        (distribution_id.clone(), distribution_state),
        (cache_id.clone(), cache_state),
    ]);
    let resources = vec![web_acl(), distribution(), cache];

    let plan = cascade_plan_for(&resources, current_states, &registry);

    assert_eq!(
        updates_for(&plan, &distribution_id).len(),
        0,
        "B should be promoted from Update to Replace"
    );
    assert_eq!(decomposed_replace_count(&plan, &distribution_id), (1, 1));
    assert!(
        replace_metadata_for(&plan, &distribution_id).create_before_destroy,
        "B must be CBD because C depends on it"
    );
    assert_eq!(updates_for(&plan, &cache_id).len(), 1);
    assert!(
        delete_blockers_for(&plan, &distribution_id).contains("cache_policy"),
        "B old delete must wait for C's consumer update"
    );

    let unresolved: HashMap<_, _> = resources
        .iter()
        .map(|resource| {
            (
                resource.id.clone(),
                crate::effect::deps::UnresolvedResource::from_pre_resolve(resource.clone()),
            )
        })
        .collect();
    let deps = crate::effect::deps::build_effect_dependency_analysis(
        plan.effects(),
        &unresolved,
        &[],
        crate::effect::deps::ScheduleInputs::Apply,
    )
    .into_deps_of();
    let index_of = |predicate: &dyn Fn(&Effect) -> bool| {
        plan.effects()
            .iter()
            .position(predicate)
            .expect("effect must exist")
    };
    let create_a =
        index_of(&|effect| matches!(effect, Effect::Create(resource) if resource.id == web_acl_id));
    let delete_a =
        index_of(&|effect| matches!(effect, Effect::Delete { id, .. } if id == &web_acl_id));
    let create_b = index_of(
        &|effect| matches!(effect, Effect::Create(resource) if resource.id == distribution_id),
    );
    let delete_b =
        index_of(&|effect| matches!(effect, Effect::Delete { id, .. } if id == &distribution_id));
    let update_c =
        index_of(&|effect| matches!(effect, Effect::Update { id, .. } if id == &cache_id));

    assert!(deps[&create_b].contains(&create_a));
    assert!(deps[&update_c].contains(&create_b));
    assert!(deps[&delete_b].contains(&update_c));
    assert!(deps[&delete_b].contains(&create_b));
    assert!(deps[&delete_a].contains(&delete_b));
}

#[test]
fn cascade_dependent_updates_anonymous_resource() {
    let registry = base_registry();
    let web_acl_id = web_acl_id();
    let distribution_id = distribution_id();
    let anonymous_distribution = Resource::new("cdn.Distribution", "main")
        .with_attribute("web_acl_arn", ref_value("web_acl", "arn"))
        .with_attribute("comment", str_value("unchanged"))
        .with_attribute("domain_name", str_value("example.cloudfront.net"));
    let current_states = HashMap::from([
        (web_acl_id.clone(), current_web_acl_state()),
        (
            distribution_id.clone(),
            current_distribution_state(ref_value("web_acl", "arn"), "unchanged"),
        ),
    ]);

    let plan = cascade_plan_for(
        &[web_acl(), anonymous_distribution],
        current_states,
        &registry,
    );

    assert_eq!(updates_for(&plan, &distribution_id).len(), 1);
    assert!(
        delete_blockers_for(&plan, &web_acl_id).contains("cdn.Distribution:main"),
        "anonymous dependent should use <resource_type>:<name> as delete blocker key"
    );
}

#[test]
fn cascade_dependent_updates_prevent_destroy_blocks_promotion() {
    let mut registry = base_registry();
    registry.insert(
        "",
        ResourceSchema::new("cdn.Distribution")
            .attribute(AttributeSchema::new("web_acl_arn", AttributeType::string()).create_only())
            .attribute(AttributeSchema::new("comment", AttributeType::string()))
            .attribute(AttributeSchema::new("domain_name", AttributeType::string())),
    );
    let web_acl_id = web_acl_id();
    let distribution_id = distribution_id();
    let mut protected_distribution = distribution();
    protected_distribution.directives.prevent_destroy = true;
    let current_states = HashMap::from([
        (web_acl_id.clone(), current_web_acl_state()),
        (
            distribution_id.clone(),
            current_distribution_state(ref_value("web_acl", "arn"), "unchanged"),
        ),
    ]);

    let plan = cascade_plan_for(
        &[web_acl(), protected_distribution],
        current_states,
        &registry,
    );

    assert!(plan.errors().iter().any(|error| {
        error.resource_id == distribution_id && error.message.contains("prevent_destroy")
    }));
    assert_eq!(decomposed_replace_count(&plan, &distribution_id), (0, 0));
}
