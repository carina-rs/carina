use super::*;

use std::collections::HashSet;

use crate::plan::{PlanErrorKind, PreventDestroyAction, ReplacementCannotCoexistError};
use crate::resource::{
    ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
    ExpandedResource as Resource, ExpandedValue as Value, ResourceIdentity,
};
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string().into()))
}

fn ref_value(binding: &str, attribute: &str) -> Value {
    Value::root_resource_ref(binding.to_string(), attribute.to_string(), vec![])
}

fn state(id: &ResourceId, attrs: &[(&str, Value)], identifier: &str) -> State {
    State::existing(
        id.clone(),
        attrs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect(),
    )
    .with_identifier(identifier)
}

fn base_schemas(subnet_vpc_id_create_only: bool) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("vpc_id", AttributeType::string()).build(),
            )
            .with_unique_name_attribute("name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let vpc_id = if subnet_vpc_id_create_only {
        AttributeSchema::required("vpc_id", AttributeType::string())
            .create_only()
            .build()
    } else {
        AttributeSchema::required("vpc_id", AttributeType::string()).build()
    };
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Subnet")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(vpc_id)
            .attribute(AttributeSchema::required("cidr_block", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("tags", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("availability_zone", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("name"))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

fn vpc_resources(create_before_destroy: bool) -> (Resource, Resource) {
    let mut unresolved = Resource::new("ec2.Vpc", "my-vpc")
        .with_root_binding("vpc")
        .with_attribute("name", string("vpc-main"))
        .with_attribute("cidr_block", string("10.1.0.0/16"));
    unresolved.directives.create_before_destroy = create_before_destroy;
    (unresolved.clone(), unresolved)
}

fn subnet_resources(_vpc_ref_create_only: bool, independent_update: bool) -> (Resource, Resource) {
    let unresolved = Resource::new("ec2.Subnet", "my-subnet")
        .with_root_binding("subnet")
        .with_attribute("name", string("subnet-main"))
        .with_attribute("vpc_id", ref_value("vpc", "vpc_id"))
        .with_attribute("cidr_block", string("10.1.1.0/24"))
        .with_attribute(
            "tags",
            string(if independent_update {
                "new-tag"
            } else {
                "old-tag"
            }),
        )
        .with_attribute("availability_zone", string("us-east-1a"));
    let managed = Resource::new("ec2.Subnet", "my-subnet")
        .with_root_binding("subnet")
        .with_attribute("name", string("subnet-main"))
        .with_attribute("vpc_id", string("vpc-old"))
        .with_attribute("cidr_block", string("10.1.1.0/24"))
        .with_attribute(
            "tags",
            string(if independent_update {
                "new-tag"
            } else {
                "old-tag"
            }),
        )
        .with_attribute("availability_zone", string("us-east-1a"));
    (managed, unresolved)
}

fn plan_for(
    managed: Vec<Resource>,
    unresolved: Vec<Resource>,
    current_states: HashMap<ResourceId, State>,
    schemas: &SchemaRegistry,
) -> Plan {
    let unresolved = ResolvedResource::collect_resolved(unresolved)
        .expect("cascade test resources must have resolved identities");
    let resolved = ResolvedResource::collect_resolved(managed)
        .expect("cascade test resources must have resolved identities");
    let (schemas, provider) = crate::provider::comparison_fixture_for_test(
        schemas,
        resolved.iter().map(|resource| &resource.id),
    );
    let plan_input_states =
        crate::resource::into_plan_input_current_map_from_states(current_states, &schemas, &[]);
    create_plan_with_cascades(
        &resolved,
        &unresolved,
        &[],
        &provider,
        crate::differ::PlanPriorStates::new(&plan_input_states, &crate::differ::Orphans::new()),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &[],
        &HashSet::new(),
    )
    .expect("test provider hints should succeed")
}

fn standard_states() -> HashMap<ResourceId, State> {
    let vpc_id = ResourceId::with_identity("ec2.Vpc", "my-vpc");
    let subnet_id = ResourceId::with_identity("ec2.Subnet", "my-subnet");
    HashMap::from([
        (
            vpc_id.clone(),
            state(
                &vpc_id,
                &[
                    ("name", string("vpc-main")),
                    ("cidr_block", string("10.0.0.0/16")),
                    ("vpc_id", string("vpc-old")),
                ],
                "vpc-old",
            ),
        ),
        (
            subnet_id.clone(),
            state(
                &subnet_id,
                &[
                    ("name", string("subnet-main")),
                    ("vpc_id", string("vpc-old")),
                    ("cidr_block", string("10.1.1.0/24")),
                    ("tags", string("old-tag")),
                    ("availability_zone", string("us-east-1a")),
                ],
                "subnet-123",
            ),
        ),
    ])
}

fn replacement_indices(plan: &Plan, id: &ResourceId) -> (usize, usize) {
    for metadata in &plan.replace_display {
        if plan.effects()[metadata.create_idx].resource_id() == id {
            return (metadata.create_idx, metadata.delete_idx);
        }
    }
    panic!(
        "replacement metadata for {id} not found: {:?}",
        plan.effects()
    );
}

fn replacement_metadata<'a>(
    plan: &'a Plan,
    id: &ResourceId,
) -> &'a crate::plan::ReplaceDisplayMetadata {
    plan.replace_display
        .iter()
        .find(|metadata| plan.effects()[metadata.create_idx].resource_id() == id)
        .unwrap_or_else(|| panic!("replacement metadata for {id} not found"))
}

fn update_for<'a>(plan: &'a Plan, id: &ResourceId) -> Option<&'a Effect> {
    plan.effects()
        .iter()
        .find(|effect| matches!(effect, Effect::Update { to, .. } if to.id == *id))
}

fn delete_blocked_by<'a>(plan: &'a Plan, id: &ResourceId) -> &'a HashSet<ResourceIdentity> {
    let (_, delete_idx) = replacement_indices(plan, id);
    match &plan.effects()[delete_idx] {
        Effect::Delete {
            blocked_by_updates, ..
        } => blocked_by_updates,
        other => panic!("expected replacement delete for {id}, got {other:?}"),
    }
}

#[test]
fn cascade_dependent_updates_adds_update_for_dependent() {
    let schemas = base_schemas(false);
    let (managed_vpc, unresolved_vpc) = vpc_resources(true);
    let (managed_subnet, unresolved_subnet) = subnet_resources(false, false);
    let vpc_id = managed_vpc.id.clone();
    let subnet_id = managed_subnet.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    let update = update_for(&plan, &subnet_id).expect("subnet update should be added");
    match update {
        Effect::Update {
            to,
            changed_attributes,
            ..
        } => {
            assert!(changed_attributes.contains(&"vpc_id".to_string()));
            assert!(matches!(
                to.get_attr("vpc_id"),
                Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
            ));
        }
        other => panic!("expected subnet update, got {other:?}"),
    }

    assert!(delete_blocked_by(&plan, &vpc_id).contains(&ResourceIdentity::new("my-subnet")));
}

#[test]
fn cascade_reuses_existing_update_for_dependent() {
    let schemas = base_schemas(false);
    let (managed_vpc, unresolved_vpc) = vpc_resources(true);
    let (managed_subnet, unresolved_subnet) = subnet_resources(false, true);
    let vpc_id = managed_vpc.id.clone();
    let subnet_id = managed_subnet.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    let updates: Vec<_> = plan
        .effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::Update { to, .. } if to.id == subnet_id))
        .collect();
    assert_eq!(updates.len(), 1);
    match updates[0] {
        Effect::Update {
            changed_attributes, ..
        } => {
            assert!(changed_attributes.contains(&"tags".to_string()));
            assert!(changed_attributes.contains(&"vpc_id".to_string()));
        }
        other => panic!("expected subnet update, got {other:?}"),
    }
    assert!(delete_blocked_by(&plan, &vpc_id).contains(&ResourceIdentity::new("my-subnet")));
}

#[test]
fn cascade_generates_replace_when_dependent_attribute_is_create_only() {
    let schemas = base_schemas(true);
    let (managed_vpc, unresolved_vpc) = vpc_resources(true);
    let (managed_subnet, unresolved_subnet) = subnet_resources(true, false);
    let vpc_id = managed_vpc.id.clone();
    let subnet_id = managed_subnet.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    assert_eq!(plan.replace_display.len(), 2);
    assert!(update_for(&plan, &subnet_id).is_none());
    assert!(delete_blocked_by(&plan, &vpc_id).contains(&ResourceIdentity::new("my-subnet")));

    let metadata = replacement_metadata(&plan, &subnet_id);
    assert!(metadata.changed_create_only.contains("vpc_id"));
    assert!(
        metadata
            .cascade_ref_hints
            .contains(&("vpc_id".to_string(), "vpc.vpc_id".to_string()))
    );
}

#[test]
fn cascade_replace_keeps_new_create_only_consumer_as_create() {
    let schemas = base_schemas(true);
    let (managed_vpc, unresolved_vpc) = vpc_resources(true);
    let (managed_subnet, unresolved_subnet) = subnet_resources(true, false);
    let vpc_id = managed_vpc.id.clone();
    let subnet_id = managed_subnet.id.clone();
    let mut states = standard_states();
    states.insert(subnet_id.clone(), State::not_found(subnet_id.clone()));

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        states,
        &schemas,
    );

    assert!(
        plan.errors().is_empty(),
        "unexpected errors: {:?}",
        plan.errors()
    );
    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Create(resource) if resource.id == subnet_id)),
        "the new consumer must remain a Create: {:?}",
        plan.effects(),
    );
    assert_eq!(plan.replace_display.len(), 1);
    assert_eq!(
        plan.effects()[replacement_metadata(&plan, &vpc_id).create_idx].resource_id(),
        &vpc_id,
    );
}

#[test]
fn cascade_merges_with_existing_replace_direct_change_plus_cascade() {
    let schemas = base_schemas(true);
    let (managed_vpc, unresolved_vpc) = vpc_resources(true);
    let (mut managed_subnet, mut unresolved_subnet) = subnet_resources(true, false);
    managed_subnet.set_attr("availability_zone", string("us-east-1b"));
    unresolved_subnet.set_attr("availability_zone", string("us-east-1b"));
    let subnet_id = managed_subnet.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    let metadata = replacement_metadata(&plan, &subnet_id);
    assert!(metadata.changed_create_only.contains("availability_zone"));
    assert!(metadata.changed_create_only.contains("vpc_id"));
    assert!(
        metadata
            .cascade_ref_hints
            .contains(&("vpc_id".to_string(), "vpc.vpc_id".to_string()))
    );
}

#[test]
fn cascade_upgrades_update_to_replace_when_ref_is_create_only() {
    let schemas = base_schemas(true);
    let (managed_vpc, unresolved_vpc) = vpc_resources(true);
    let (managed_subnet, unresolved_subnet) = subnet_resources(true, true);
    let subnet_id = managed_subnet.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    assert!(update_for(&plan, &subnet_id).is_none());
    assert!(
        replacement_metadata(&plan, &subnet_id)
            .changed_create_only
            .contains("vpc_id")
    );
}

#[test]
fn auto_detect_create_before_destroy_when_resource_has_dependents() {
    let schemas = base_schemas(false);
    let (managed_vpc, unresolved_vpc) = vpc_resources(false);
    let (managed_subnet, unresolved_subnet) = subnet_resources(false, false);
    let vpc_id = managed_vpc.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    let metadata = replacement_metadata(&plan, &vpc_id);
    assert!(metadata.create_before_destroy);
    assert!(metadata.temporary_name.is_some());
}

#[test]
fn cascade_prevent_destroy_blocks_promotion_to_replace() {
    let schemas = base_schemas(true);
    let (managed_vpc, unresolved_vpc) = vpc_resources(true);
    let (managed_subnet, mut unresolved_subnet) = subnet_resources(true, false);
    unresolved_subnet.directives.prevent_destroy = true;
    let subnet_id = managed_subnet.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    assert!(plan.has_errors());
    assert_eq!(plan.errors()[0].resource_id, subnet_id);
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::PreventDestroy {
            action: PreventDestroyAction::CascadeReplace
        }
    );
    assert!(
        plan.replace_display
            .iter()
            .all(|metadata| { plan.effects()[metadata.create_idx].resource_id() != &subnet_id })
    );
}

#[test]
fn auto_promote_with_missing_unique_name_attribute_emits_plan_error() {
    let mut schemas = base_schemas(false);
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let (managed_vpc, unresolved_vpc) = vpc_resources(false);
    let (managed_subnet, unresolved_subnet) = subnet_resources(false, false);
    let vpc_id = managed_vpc.id.clone();

    let plan = plan_for(
        vec![managed_vpc, managed_subnet],
        vec![unresolved_vpc, unresolved_subnet],
        standard_states(),
        &schemas,
    );

    assert!(plan.has_errors());
    assert_eq!(plan.errors()[0].resource_id, vpc_id);
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::ReplacementCannotCoexist(ReplacementCannotCoexistError {
            resource_type: "ec2.Vpc".to_string(),
            resource_identity: "vpc".to_string(),
        })
    );
    assert_eq!(
        plan.errors()[0]
            .display_with_project_dir(std::path::Path::new("."))
            .to_string(),
        "ec2.Vpc.my-vpc: resource type 'ec2.Vpc' does not support create_before_destroy: the replacement cannot coexist with the original"
    );
}

#[test]
fn chained_cbd_anonymous_middle_node_auto_promoted() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("test.Producer")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("shape", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("producer_id", AttributeType::string()).build(),
            )
            .with_unique_name_attribute("name"))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "",
        (ResourceSchema::new("test.Middle")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("a_id", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::provider_populated("b_id", AttributeType::string()).build())
            .with_unique_name_attribute("name"))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "",
        (ResourceSchema::new("test.Consumer")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::optional("b_id", AttributeType::string()).build())
            .with_unique_name_attribute("name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut producer = Resource::new("test.Producer", "producer")
        .with_root_binding("a")
        .with_attribute("name", string("producer"))
        .with_attribute("shape", string("new"));
    producer.directives.create_before_destroy = true;
    let unresolved_producer = producer.clone();

    let managed_middle = Resource::new("test.Middle", "anon-b")
        .with_attribute("name", string("middle"))
        .with_attribute("a_id", string("a-old"));
    let unresolved_middle = Resource::new("test.Middle", "anon-b")
        .with_attribute("name", string("middle"))
        .with_attribute("a_id", ref_value("a", "producer_id"));

    let managed_consumer = Resource::new("test.Consumer", "consumer")
        .with_root_binding("c")
        .with_attribute("name", string("consumer"))
        .with_attribute("b_id", string("b-old"));
    let unresolved_consumer = Resource::new("test.Consumer", "consumer")
        .with_root_binding("c")
        .with_attribute("name", string("consumer"))
        .with_attribute("b_id", ref_value("anon-b", "b_id"));

    let producer_id = producer.id.clone();
    let middle_id = managed_middle.id.clone();
    let consumer_id = managed_consumer.id.clone();
    let states = HashMap::from([
        (
            producer_id.clone(),
            state(
                &producer_id,
                &[
                    ("name", string("producer")),
                    ("shape", string("old")),
                    ("producer_id", string("a-old")),
                ],
                "a-old",
            ),
        ),
        (
            middle_id.clone(),
            state(
                &middle_id,
                &[
                    ("name", string("middle")),
                    ("a_id", string("a-old")),
                    ("b_id", string("b-old")),
                ],
                "b-old",
            ),
        ),
        (
            consumer_id.clone(),
            state(
                &consumer_id,
                &[("name", string("consumer")), ("b_id", string("b-old"))],
                "c-old",
            ),
        ),
    ]);

    let plan = plan_for(
        vec![producer, managed_middle, managed_consumer],
        vec![unresolved_producer, unresolved_middle, unresolved_consumer],
        states,
        &schemas,
    );

    assert_eq!(plan.replace_display.len(), 2);
    assert!(replacement_metadata(&plan, &middle_id).create_before_destroy);
    assert!(update_for(&plan, &consumer_id).is_some());
    assert!(
        delete_blocked_by(&plan, &producer_id).contains(&ResourceIdentity::new("anon-b")),
        "producer delete should wait for anonymous middle replacement"
    );
    assert!(
        delete_blocked_by(&plan, &middle_id).contains(&ResourceIdentity::new("consumer")),
        "anonymous middle delete should wait for downstream consumer update"
    );
}

const COLLIDING_BINDING: &str = "mock_test_replaceable_deadbeef";

struct BindingIdentityCollisionFixture {
    plan: Plan,
    unresolved: Vec<Resource>,
    anonymous_id: ResourceId,
    consumer_id: ResourceId,
}

fn binding_identity_collision_fixture() -> BindingIdentityCollisionFixture {
    let mut directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    directory.push("tests/fixtures/dependency_key_collision");
    let parsed = crate::config_loader::parse_directory(
        &directory,
        &crate::parser::ProviderContext::default(),
    )
    .expect("multi-file collision fixture parses");
    let parsed = crate::module_resolver::resolve_modules_providerless(parsed, &directory)
        .expect("multi-file collision fixture crosses root expansion");
    let mut unresolved = parsed.resources;

    let anonymous = unresolved
        .iter_mut()
        .find(|resource| {
            resource.id.resource_type == "test.replaceable" && resource.binding.is_none()
        })
        .expect("fixture contains the anonymous replaceable resource");
    anonymous
        .id
        .set_identity(ResourceIdentity::new(COLLIDING_BINDING));
    let anonymous_id = anonymous.id.clone();

    let consumer_id = unresolved
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|binding| binding.as_str() == "consumer")
        })
        .expect("fixture contains the consumer binding")
        .id
        .clone();

    let mut managed = unresolved.clone();
    let managed_consumer = managed
        .iter_mut()
        .find(|resource| resource.id == consumer_id)
        .expect("managed fixture contains consumer");
    managed_consumer.set_attr("anchor_id", string("anchor-id"));

    let anchor_id = managed
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|binding| binding.as_str() == COLLIDING_BINDING)
        })
        .expect("fixture contains the colliding let binding")
        .id
        .clone();

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.replaceable")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::required("shape", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("name"))
        .validate()
        .expect("replaceable fixture schema is valid"),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("test.anchor")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("identifier", AttributeType::string()).build(),
            )
            .validate()
            .expect("anchor fixture schema is valid"),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("test.consumer")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(AttributeSchema::required("anchor_id", AttributeType::string()).build())
            .attribute(AttributeSchema::required("note", AttributeType::string()).build())
            .validate()
            .expect("consumer fixture schema is valid"),
    );

    let states = HashMap::from([
        (
            anonymous_id.clone(),
            state(
                &anonymous_id,
                &[("name", string("anonymous")), ("shape", string("old"))],
                "anonymous-id",
            ),
        ),
        (
            anchor_id.clone(),
            state(&anchor_id, &[("name", string("anchor"))], "anchor-id"),
        ),
        (
            consumer_id.clone(),
            state(
                &consumer_id,
                &[
                    ("name", string("consumer")),
                    ("anchor_id", string("anchor-id")),
                    ("note", string("old")),
                ],
                "consumer-id",
            ),
        ),
    ]);

    let plan = plan_for(managed, unresolved.clone(), states, &schemas);
    BindingIdentityCollisionFixture {
        plan,
        unresolved,
        anonymous_id,
        consumer_id,
    }
}

#[test]
fn binding_spelling_collision_does_not_promote_anonymous_replace_to_cbd() {
    let fixture = binding_identity_collision_fixture();
    let metadata = replacement_metadata(&fixture.plan, &fixture.anonymous_id);

    assert!(
        !metadata.create_before_destroy,
        "a dependency on the typed let binding must not promote an unrelated anonymous identity"
    );
}

#[test]
fn binding_spelling_collision_does_not_link_apply_dag_to_anonymous_replace() {
    let fixture = binding_identity_collision_fixture();
    let anonymous_create = fixture
        .plan
        .effects()
        .iter()
        .position(|effect| {
            matches!(effect, Effect::Create(resource) if resource.id == fixture.anonymous_id)
        })
        .expect("fixture plan contains anonymous replacement create");
    let consumer_update = fixture
        .plan
        .effects()
        .iter()
        .position(
            |effect| matches!(effect, Effect::Update { to, .. } if to.id == fixture.consumer_id),
        )
        .expect("fixture plan contains consumer update");
    let unresolved = fixture
        .unresolved
        .into_iter()
        .map(|resource| {
            (
                resource.id.clone(),
                crate::effect::deps::UnresolvedResource::from_pre_resolve(resource),
            )
        })
        .collect();
    let analysis = crate::effect::deps::build_effect_dependency_analysis(
        fixture.plan.effects(),
        &unresolved,
        &[],
        crate::effect::deps::ScheduleInputs::Apply,
    )
    .expect("fixture dependency graph is valid");

    assert!(
        !analysis
            .deps_of(consumer_update)
            .expect("consumer has a dependency set")
            .contains(&anonymous_create),
        "the apply DAG must not resolve a binding through an equal-spelling resource identity"
    );
}
