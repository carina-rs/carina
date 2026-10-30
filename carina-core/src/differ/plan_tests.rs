use super::{create_plan_for_tests as create_plan, *};

use indexmap::IndexMap;
use std::collections::{BTreeSet, HashSet};

use crate::explicit::{ExplicitFields, PendingPaths};
use crate::plan::{
    PlanErrorKind, PreventDestroyAction, ReplacementCannotCoexistError, SchemaNotRegisteredError,
};
use crate::resource::{ConcreteValue, ResolvedResourceId, Resource, ResourceIdentity};

fn existing_state(id: ResourceId, attributes: HashMap<String, Value>) -> State {
    let identifier = id
        .identity_str()
        .expect("fixture resource identity must be resolved")
        .to_string();
    State::existing(id, attributes).with_identifier(identifier)
}

fn ignore_path(parts: &[&str]) -> crate::resource::ParsedIgnorePath {
    crate::resource::ParsedIgnorePath::new(
        parts
            .iter()
            .map(|part| crate::resource::ParsedIgnorePathSegment::Attribute((*part).to_string()))
            .collect(),
    )
}

#[test]
fn plan_records_readable_ignored_drift_without_an_update() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut desired = Resource::with_provider("mock", "Widget", "example", None).with_attribute(
        "comment",
        Value::Concrete(ConcreteValue::String("recorded".into())),
    );
    desired.directives.ignore_changes = vec![ignore_path(&["comment"])];
    let id = desired.id.clone();
    let schema = ResourceSchema::new("Widget")
        .attribute(AttributeSchema::optional("comment", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().expect("valid schema"));
    let current = crate::resource::into_plan_input_current_map(
        HashMap::from([(
            id.clone(),
            Current::from_state(existing_state(
                id.clone(),
                HashMap::from([(
                    "comment".to_string(),
                    Value::Concrete(ConcreteValue::String("external".into())),
                )]),
            )),
        )]),
        &schemas,
        std::slice::from_ref(&desired),
    );

    let plan = create_plan(
        &[desired],
        &[],
        &HintProvider { hints: Vec::new() },
        &current,
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(plan.effects().is_empty());
    assert_eq!(plan.ignored_change_summaries().len(), 1);
    assert_eq!(plan.ignored_change_summaries()[0].resource_id(), &id);
    assert_eq!(plan.ignored_change_summaries()[0].count(), 1);
}

#[test]
fn plan_reports_typed_error_when_changed_parent_contains_ignored_write_only_leaf() {
    use crate::schema::{AttributeSchema, AttributeType, StructField};

    let config = |secret: Option<&str>, memory: i64| {
        let mut values = IndexMap::new();
        if let Some(secret) = secret {
            values.insert(
                "secret".to_string(),
                Value::Concrete(ConcreteValue::String(secret.into())),
            );
        }
        values.insert(
            "memory".to_string(),
            Value::Concrete(ConcreteValue::Int(memory)),
        );
        Value::Concrete(ConcreteValue::Map(values))
    };
    let mut desired = Resource::with_provider("mock", "Widget", "example", None)
        .with_attribute("config", config(Some("recorded"), 256));
    desired.directives.ignore_changes = vec![ignore_path(&["config", "secret"])];
    let id = desired.id.clone();
    let schema = ResourceSchema::new("Widget").attribute(
        AttributeSchema::optional(
            "config",
            AttributeType::struct_(
                "Config",
                vec![
                    StructField::optional("secret", AttributeType::string())
                        .write_only()
                        .build(),
                    StructField::optional("memory", AttributeType::int()).build(),
                ],
            ),
        )
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().expect("valid schema"));
    let current = crate::resource::into_plan_input_current_map(
        HashMap::from([(
            id.clone(),
            Current::from_state(existing_state(
                id.clone(),
                HashMap::from([("config".to_string(), config(None, 128))]),
            )),
        )]),
        &schemas,
        std::slice::from_ref(&desired),
    );

    let plan = create_plan(
        &[desired],
        &[],
        &HintProvider { hints: Vec::new() },
        &current,
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(plan.effects().is_empty());
    assert!(matches!(
        &plan.errors()[0].kind,
        PlanErrorKind::IgnoredWriteOnlyValueUnavailable { attribute, path }
            if attribute == "config" && path == "config.secret"
    ));
}

#[test]
fn missing_identifier_current_produces_existing_plan_error() {
    let resource = Resource::new("test.resource", "managed");
    let id = resource.id.clone();
    let managed_current = HashMap::from([(
        id.clone(),
        crate::resource::Current::from_state(State::existing(id.clone(), HashMap::new())),
    )]);
    let plan_current = crate::resource::into_plan_input_current_map(
        managed_current,
        &SchemaRegistry::new(),
        std::slice::from_ref(&resource),
    );

    let plan = create_plan(
        &[resource],
        &[],
        &HintProvider { hints: Vec::new() },
        &plan_current,
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.errors().len(), 1);
    assert_eq!(plan.errors()[0].resource_id, id);
    assert_eq!(
        plan.errors()[0].kind,
        PlanErrorKind::MissingProviderIdentifier
    );
}

#[test]
fn current_variant_determines_create_update_or_plan_error() {
    let desired = Resource::new("test.resource", "managed")
        .with_attribute("name", Value::Concrete(ConcreteValue::String("new".into())));
    let id = desired.id.clone();
    let make_plan = |current| {
        let current = crate::resource::into_plan_input_current_map(
            HashMap::from([(id.clone(), current)]),
            &SchemaRegistry::new(),
            std::slice::from_ref(&desired),
        );
        create_plan(
            std::slice::from_ref(&desired),
            &[],
            &HintProvider { hints: Vec::new() },
            &current,
            &HashMap::new(),
            &SchemaRegistry::new(),
            &empty_lifted_saved_attrs(),
            &HashMap::new(),
            &Orphans::new(),
            &[],
        )
    };

    let absent = make_plan(Current::absent(id.clone()));
    assert_eq!(absent.summary().create, 1);
    assert!(absent.errors().is_empty());

    let present = make_plan(Current::from_state(
        State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("old".into())),
            )]),
        )
        .with_identifier("provider-id"),
    ));
    assert_eq!(present.summary().update, 1);
    assert!(present.errors().is_empty());

    let missing_identifier = make_plan(Current::from_state(State::existing(
        id.clone(),
        HashMap::new(),
    )));
    assert!(missing_identifier.effects().is_empty());
    assert_eq!(missing_identifier.errors().len(), 1);
    assert_eq!(
        missing_identifier.errors()[0].kind,
        PlanErrorKind::MissingProviderIdentifier
    );
}

#[test]
fn desired_in_flight_plans_resume_never_create() {
    use crate::effect::Effect;
    use crate::provider::{CreateAttempt, InFlightCreate, PendingToken, Sent};

    let desired = Resource::new("test.resource", "managed");
    let id = desired.id.clone();
    let pending = InFlightCreate::from_pending(CreateAttempt::new(id.clone()).pending(
        PendingToken::new("secret-provider-token"),
        Sent::AllRequestAttributes,
        1,
    ));
    let started_at = pending.started_at().clone();
    let current = crate::resource::into_plan_input_current_map(
        HashMap::from([(id.clone(), Current::create_in_flight(pending))]),
        &SchemaRegistry::new(),
        std::slice::from_ref(&desired),
    );

    let plan = create_plan(
        &[desired],
        &[],
        &HintProvider { hints: Vec::new() },
        &current,
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(matches!(
        plan.effects(),
        [Effect::ResumeCreate { resource, started_at: effect_started_at }]
            if resource.id == id && effect_started_at == &started_at
    ));
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Create(_)))
    );
}

#[test]
fn saved_resume_contains_no_token() {
    use crate::provider::{CreateAttempt, InFlightCreate, PendingToken, Sent};

    let desired = Resource::new("test.resource", "managed");
    let id = desired.id.clone();
    let pending = InFlightCreate::from_pending(CreateAttempt::new(id.clone()).pending(
        PendingToken::new("must-not-enter-plan"),
        Sent::AllRequestAttributes,
        1,
    ));
    let current = crate::resource::into_plan_input_current_map(
        HashMap::from([(id, Current::create_in_flight(pending))]),
        &SchemaRegistry::new(),
        std::slice::from_ref(&desired),
    );
    let plan = create_plan(
        &[desired],
        &[],
        &HintProvider { hints: Vec::new() },
        &current,
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    let encoded = serde_json::to_string(&plan).expect("plan serializes");
    assert!(!encoded.contains("must-not-enter-plan"), "{encoded}");
}

#[test]
fn in_flight_orphan_is_create_in_flight_orphan_error() {
    use crate::provider::{CreateAttempt, InFlightCreate, PendingToken, Sent};

    let id = ResourceId::with_identity("test.resource", "orphan");
    let pending = InFlightCreate::from_pending(CreateAttempt::new(id.clone()).pending(
        PendingToken::new("opaque"),
        Sent::AllRequestAttributes,
        1,
    ));
    let orphans = Orphans::from([Orphan::create_in_flight(
        pending,
        Some("orphan".to_string()),
        Directives::default(),
    )]);
    let plan = create_plan(
        &[],
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    assert!(plan.effects().is_empty());
    assert_eq!(plan.errors().len(), 1);
    assert_eq!(plan.errors()[0].resource_id, id);
    assert_eq!(plan.errors()[0].kind, PlanErrorKind::CreateInFlightOrphan);
}

#[test]
fn changed_provider_instance_rejects_resume() {
    use crate::effect::Effect;
    use crate::provider::{CreateAttempt, InFlightCreate, PendingToken, Sent};

    let desired = Resource::with_provider(
        "mock",
        "test.resource",
        "managed",
        Some("new-route".to_string()),
    );
    let desired_id = desired.id.clone();
    let persisted_id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "managed",
        Some("issuing-route".to_string()),
    );
    let pending = InFlightCreate::from_pending(CreateAttempt::new(persisted_id.clone()).pending(
        PendingToken::new("opaque"),
        Sent::AllRequestAttributes,
        1,
    ));
    let current = crate::resource::into_plan_input_current_map(
        HashMap::from([(desired_id.clone(), Current::create_in_flight(pending))]),
        &SchemaRegistry::new(),
        std::slice::from_ref(&desired),
    );

    let plan = create_plan(
        &[desired],
        &[],
        &HintProvider { hints: Vec::new() },
        &current,
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Create(_) | Effect::ResumeCreate { .. })),
        "a routing mismatch must leave the persisted continuation untouched"
    );
    assert_eq!(plan.errors().len(), 1);
    assert_eq!(
        plan.errors()[0].kind,
        PlanErrorKind::ResumeCreateRoutingMismatch {
            persisted: persisted_id,
        }
    );
}

fn provider_ready(resource: Resource) -> crate::provider::ProviderReadyResource {
    let bindings = crate::binding_index::ResolvedBindings::default();
    let gate = crate::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = crate::schema::SchemaRegistry::new();
    let context = crate::executor::ProviderPreparationContext::new(
        &bindings,
        &gate,
        &[],
        &crate::provider::NoopNormalizer,
        &[],
        &schemas,
    );
    futures::executor::block_on(crate::executor::prepare_provider_ready_resource(
        resource, &context,
    ))
    .expect("test resource should pass provider preparation")
}

/// Build an `ExplicitFields::Struct` whose children are all `Leaf` —
/// the shape `state v5 → v6` reads produce, and a convenient way to
/// express "user previously authored these top-level keys" in tests
/// that pre-date the nested authoring tree.
fn explicit_top_level(keys: &[&str]) -> ExplicitFields {
    ExplicitFields::Struct {
        children: keys
            .iter()
            .map(|k| ((*k).to_string(), ExplicitFields::Leaf))
            .collect(),
    }
}

struct HintProvider {
    hints: Vec<crate::wait::BindingPattern>,
}

impl crate::provider::Provider for HintProvider {
    fn name(&self) -> &str {
        "hint"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: crate::provider::ReadRequest,
    ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<crate::provider::ReadOutcome>>
    {
        Box::pin(async { panic!("unexpected read") })
    }

    fn read_data_source(
        &self,
        _resource: &crate::provider::ProviderReadyDataSource,
    ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<State>> {
        Box::pin(async { panic!("unexpected read_data_source") })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: crate::provider::CreateRequest,
        _attempt: crate::provider::CreateAttempt,
    ) -> crate::provider::BoxFuture<
        '_,
        Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>,
    > {
        Box::pin(async { panic!("unexpected create") })
    }

    fn resume_create(
        &self,
        _request: crate::provider::ResumeCreateRequest,
    ) -> crate::provider::BoxFuture<
        '_,
        Result<crate::provider::ResumeOutcome, crate::provider::ResumeFailure>,
    > {
        Box::pin(async { panic!("unexpected resume_create") })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: crate::provider::UpdateRequest,
    ) -> crate::provider::BoxFuture<
        '_,
        crate::provider::ProviderResult<crate::provider::UpdateOutcome>,
    > {
        Box::pin(async { panic!("unexpected update") })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: crate::provider::DeleteRequest,
    ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<()>> {
        Box::pin(async { panic!("unexpected delete") })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> crate::provider::ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }

    fn satisfier_hint(
        &self,
        _target_id: &ResourceId,
        _attr_path: &crate::wait::predicate::AttrPath,
    ) -> crate::provider::ProviderResult<Vec<crate::wait::BindingPattern>> {
        Ok(self.hints.clone())
    }
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

#[test]
fn create_before_destroy_generates_temporary_name_for_unique_name_attribute() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute(
            "bucket_name",
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        )
        .with_attribute(
            "object_lock_enabled",
            Value::Concrete(ConcreteValue::Bool(true)),
        );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        existing_state(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("s3.Bucket")
            .attribute(
                AttributeSchema::optional("bucket_name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("object_lock_enabled", AttributeType::bool())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("bucket_name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    let metadata =
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"));
    let temp = metadata
        .temporary_name
        .as_ref()
        .expect("Should have temporary_name for create_before_destroy with unique_name_attribute");
    assert_eq!(temp.attribute, "bucket_name");
    assert_eq!(temp.original_value, "my-bucket");
    assert!(
        temp.temporary_value.starts_with("my-bucket-"),
        "Temporary value '{}' should start with 'my-bucket-'",
        temp.temporary_value
    );
    assert_eq!(temp.temporary_value.len(), "my-bucket-".len() + 8);
    // bucket_name is create-only, so can_rename should be false
    assert!(!temp.can_rename);
    match &plan.effects()[metadata.create_idx] {
        Effect::Create(to) => assert_eq!(
            to.get_attr("bucket_name"),
            Some(&Value::Concrete(ConcreteValue::String(
                temp.temporary_value.clone().into()
            )))
        ),
        other => panic!("Expected replacement Create, got {:?}", other),
    }
}

#[test]
fn create_before_destroy_generates_temporary_name_with_can_rename() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("logs.LogGroup", "my-log-group")
        .with_attribute(
            "log_group_name".to_string(),
            Value::Concrete(ConcreteValue::String("my-log-group".into())),
        )
        .with_attribute(
            "kms_key_id",
            Value::Concrete(ConcreteValue::String("new-key".into())),
        );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "log_group_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-log-group".into())),
    );
    attrs.insert(
        "kms_key_id".to_string(),
        Value::Concrete(ConcreteValue::String("old-key".into())),
    );
    current_states.insert(
        ResourceId::with_identity("logs.LogGroup", "my-log-group"),
        existing_state(
            ResourceId::with_identity("logs.LogGroup", "my-log-group"),
            attrs,
        ),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("logs.LogGroup")
            .attribute(
                // log_group_name is NOT create-only in this test (can be renamed)
                AttributeSchema::optional("log_group_name", AttributeType::string()).build(),
            )
            .attribute(
                AttributeSchema::optional("kms_key_id", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("log_group_name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    let temp = replacement_metadata(
        &plan,
        &ResourceId::with_identity("logs.LogGroup", "my-log-group"),
    )
    .temporary_name
    .as_ref()
    .expect("Should have temporary_name");
    assert_eq!(temp.attribute, "log_group_name");
    assert_eq!(temp.original_value, "my-log-group");
    // log_group_name is not create-only, so can_rename should be true
    assert!(temp.can_rename);
}

#[test]
fn no_temporary_name_without_create_before_destroy() {
    use crate::schema::{AttributeSchema, AttributeType};

    // Default directives (create_before_destroy = false)
    let resources = vec![
        Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "bucket_name",
                Value::Concrete(ConcreteValue::String("my-bucket".into())),
            )
            .with_attribute(
                "object_lock_enabled",
                Value::Concrete(ConcreteValue::Bool(true)),
            ),
    ];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        existing_state(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("s3.Bucket")
            .attribute(
                AttributeSchema::optional("bucket_name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("object_lock_enabled", AttributeType::bool())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("bucket_name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"))
            .temporary_name
            .is_none(),
        "Should not have temporary_name without create_before_destroy"
    );
}

#[test]
fn no_temporary_name_when_name_prefix_is_used() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute(
            "bucket_name",
            Value::Concrete(ConcreteValue::String("my-app-abc12345".into())),
        )
        .with_attribute(
            "object_lock_enabled",
            Value::Concrete(ConcreteValue::Bool(true)),
        );
    resource.directives.create_before_destroy = true;
    // Simulate that name_prefix was used
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-abc12345".into())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        existing_state(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("s3.Bucket")
            .attribute(
                AttributeSchema::optional("bucket_name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("object_lock_enabled", AttributeType::bool())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("bucket_name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"))
            .temporary_name
            .is_none(),
        "Should not generate temporary_name when name_prefix is used"
    );
}

#[test]
fn create_before_destroy_without_unique_name_attribute_emits_plan_error() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("ec2.Vpc", "my-vpc").with_attribute(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
    );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        existing_state(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_conflicting_replacement())
        .validate()
        .expect("schema fixture must be valid"),
        // No unique_name_attribute set.
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(plan.effects().is_empty());
    assert!(plan.has_errors());
    assert_eq!(
        plan.errors()[0].resource_id,
        ResourceId::with_identity("ec2.Vpc", "my-vpc")
    );
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::ReplacementCannotCoexist(ReplacementCannotCoexistError {
            resource_type: "ec2.Vpc".to_string(),
            resource_identity: "my-vpc".to_string(),
        })
    );
    assert_eq!(
        plan.errors()[0].to_string(),
        "ec2.Vpc.my-vpc: resource type 'ec2.Vpc' does not support create_before_destroy: the replacement cannot coexist with the original"
    );
}

#[test]
fn create_before_destroy_with_coexisting_replacement_uses_no_temporary_name() {
    use crate::schema::{AttributeSchema, AttributeType};

    let mut resource = Resource::new("ec2.Vpc", "my-vpc").with_attribute(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
    );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        existing_state(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_coexisting_replacement())
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(!plan.has_errors(), "unexpected errors: {:?}", plan.errors());
    assert_eq!(plan.effects().len(), 2);
    let metadata = replacement_metadata(&plan, &ResourceId::with_identity("ec2.Vpc", "my-vpc"));
    assert!(metadata.create_before_destroy);
    assert!(
        metadata.create_idx < metadata.delete_idx,
        "create effect must be ordered before delete for CBD"
    );
    assert!(
        metadata.temporary_name.is_none(),
        "coexisting replacements must not generate a temporary name"
    );
    match &plan.effects()[metadata.create_idx] {
        Effect::Create(to) => assert_eq!(
            to.get_attr("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.1.0.0/16".into()
            )))
        ),
        other => panic!("Expected replacement Create, got {:?}", other),
    }
    assert!(matches!(
        &plan.effects()[metadata.delete_idx],
        Effect::Delete { .. }
    ));
}

#[test]
fn create_before_destroy_with_missing_schema_reports_schema_not_registered() {
    let resource = Resource::new("ec2.Vpc", "my-vpc");
    let from = existing_state(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        HashMap::new(),
    );
    let schemas = SchemaRegistry::new();

    let err = plan::temporary_name_for_cbd(&resource, &from, &schemas).unwrap_err();

    assert_eq!(
        PlanErrorKind::from(err),
        PlanErrorKind::SchemaNotRegistered(SchemaNotRegisteredError {
            resource_type: "ec2.Vpc".to_string(),
            resource_identity: "my-vpc".to_string(),
        })
    );
}

#[test]
fn no_temporary_name_when_unique_name_attribute_changes() {
    use crate::schema::{AttributeSchema, AttributeType};

    // unique_name_attribute itself changed: old-bucket → new-bucket
    // No temporary name needed since names are already different
    let mut resource = Resource::new("s3.Bucket", "my-bucket")
        .with_attribute(
            "bucket_name",
            Value::Concrete(ConcreteValue::String("new-bucket".into())),
        )
        .with_attribute(
            "object_lock_enabled",
            Value::Concrete(ConcreteValue::Bool(true)),
        );
    resource.directives.create_before_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("old-bucket".into())),
    );
    attrs.insert(
        "object_lock_enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        existing_state(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("s3.Bucket")
            .attribute(
                AttributeSchema::optional("bucket_name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("object_lock_enabled", AttributeType::bool())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("bucket_name"))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 2);
    assert!(
        replacement_metadata(&plan, &ResourceId::with_identity("s3.Bucket", "my-bucket"))
            .temporary_name
            .is_none(),
        "Should not generate temporary_name when unique_name_attribute value changes"
    );
}

#[test]
fn diff_detects_attribute_removal_with_prev_desired_keys() {
    // User previously had "region" and "tags" in .crn, now only has "region"
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    current_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        )]))),
    );
    let current = existing_state(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

    // Previous desired state had both "region" and "tags"
    let prev_explicit = explicit_top_level(&["region", "tags"]);

    let result = diff_test(&desired, &current, None, Some(&prev_explicit), None);
    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"tags".to_string()),
                "Should detect 'tags' removal, got: {:?}",
                changed_attributes
            );
        }
        _ => panic!("Expected Update, got {:?}", result),
    }
}

#[test]
fn diff_ignores_attributes_not_in_prev_desired_keys() {
    // Current state has "arn" and "region" from provider, but user only ever
    // specified "region" — "arn" was never in prev_desired_keys
    let desired = Resource::new("s3.Bucket", "test");

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    current_attrs.insert(
        "arn".to_string(),
        Value::Concrete(ConcreteValue::String("arn:aws:s3:::test".into())),
    );
    let current = existing_state(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

    // User previously only specified "region", not "arn"
    let prev_explicit = explicit_top_level(&["region"]);

    let result = diff_test(&desired, &current, None, Some(&prev_explicit), None);
    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"region".to_string()),
                "Should detect 'region' removal"
            );
            assert!(
                !changed_attributes.contains(&"arn".to_string()),
                "Should NOT detect 'arn' removal since it was never in desired"
            );
        }
        _ => panic!("Expected Update, got {:?}", result),
    }
}

#[test]
fn server_default_struct_field_does_not_appear_in_diff() {
    // The user wrote `lifecycle_configuration { rules: [...] }` but did
    // NOT write `transition_default_minimum_object_size`. AWS returns
    // the latter in `current` as a server-side default. The differ must
    // project current through `prev_explicit` and skip the server-only
    // leaf — no `lifecycle_configuration` change should be reported.
    use indexmap::IndexMap;

    let mut desired_lc = IndexMap::new();
    desired_lc.insert(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![{
            let mut rule = IndexMap::new();
            rule.insert(
                "id".to_string(),
                Value::Concrete(ConcreteValue::String("expire".into())),
            );
            Value::Concrete(ConcreteValue::Map(rule))
        }])),
    );
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "lifecycle_configuration",
        Value::Concrete(ConcreteValue::Map(desired_lc.clone())),
    );

    let mut current_lc = desired_lc;
    current_lc.insert(
        "transition_default_minimum_object_size".to_string(),
        Value::Concrete(ConcreteValue::String("all_storage_classes_128K".into())),
    );
    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "lifecycle_configuration".to_string(),
        Value::Concrete(ConcreteValue::Map(current_lc)),
    );
    let saved_attrs = current_attrs.clone();
    let current = existing_state(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

    // prev_explicit reflects what the user wrote: lifecycle_configuration > rules
    let prev_explicit = ExplicitFields::Struct {
        children: std::collections::HashMap::from([(
            "lifecycle_configuration".to_string(),
            ExplicitFields::Struct {
                children: std::collections::HashMap::from([(
                    "rules".to_string(),
                    ExplicitFields::List {
                        element: Box::new(ExplicitFields::Struct {
                            children: std::collections::HashMap::from([(
                                "id".to_string(),
                                ExplicitFields::Leaf,
                            )]),
                        }),
                    },
                )]),
            },
        )]),
    };

    let result = diff_test(
        &desired,
        &current,
        Some(&saved_attrs),
        Some(&prev_explicit),
        None,
    );
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Server-side default struct leaf must not surface in diff, got: {:?}",
        result
    );
}

#[test]
fn nested_authored_field_removal_produces_update_patch_without_removed_field() {
    let distribution_id =
        ResourceId::with_identity("awscc.cloudfront.Distribution", "distribution");
    let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    )])));
    let desired = Resource::new("awscc.cloudfront.Distribution", "distribution")
        .with_attribute("distribution_config", desired_config);

    let current_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "web_acl_id".to_string(),
            Value::Concrete(ConcreteValue::String("arn:aws:wafv2:acl/test".into())),
        ),
    ])));
    let current_attrs = HashMap::from([("distribution_config".to_string(), current_config)]);
    let current = existing_state(distribution_id.clone(), current_attrs.clone());
    let saved_attrs = HashMap::from([(distribution_id, current_attrs)]);
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "distribution_config".to_string(),
            ExplicitFields::Struct {
                children: HashMap::from([
                    ("enabled".to_string(), ExplicitFields::Leaf),
                    ("web_acl_id".to_string(), ExplicitFields::Leaf),
                ]),
            },
        )]),
    };

    let result = diff_test(
        &desired,
        &current,
        saved_attrs.get(&desired.id),
        Some(&prev_explicit),
        None,
    );
    let Diff::Update {
        from,
        to,
        changed_attributes,
        ..
    } = result
    else {
        panic!("Expected nested authored removal to produce Update");
    };

    assert_eq!(changed_attributes, vec!["distribution_config".to_string()]);
    let schema = crate::schema::ResourceSchema::new("awscc.cloudfront.Distribution");
    let patch = crate::provider::build_update_patch(
        &changed_attributes,
        &provider_ready(to),
        &from,
        &schema,
    )
    .expect("fixture update patch is valid");
    assert_eq!(patch.ops.len(), 1);
    let crate::provider::PatchOp::Replace {
        value: Value::Concrete(ConcreteValue::Map(patch_config)),
        ..
    } = &patch.ops[0]
    else {
        panic!("Expected replacement value to be the desired distribution config");
    };
    assert!(
        !patch_config.contains_key("web_acl_id"),
        "Update patch must not merge the removed nested field back into the desired value"
    );
}

#[test]
fn saved_server_default_nested_field_not_authored_remains_no_change() {
    let bucket_id = ResourceId::with_identity("s3.Bucket", "test");
    let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "rule".to_string(),
        Value::Concrete(ConcreteValue::String("expire".into())),
    )])));
    let desired = Resource::new("s3.Bucket", "test")
        .with_attribute("lifecycle_configuration", desired_config);

    let current_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "rule".to_string(),
            Value::Concrete(ConcreteValue::String("expire".into())),
        ),
        (
            "transition_default_minimum_object_size".to_string(),
            Value::Concrete(ConcreteValue::String("all_storage_classes_128K".into())),
        ),
    ])));
    let current_attrs = HashMap::from([("lifecycle_configuration".to_string(), current_config)]);
    let current = existing_state(bucket_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "lifecycle_configuration".to_string(),
            ExplicitFields::Struct {
                children: HashMap::from([("rule".to_string(), ExplicitFields::Leaf)]),
            },
        )]),
    };

    let result = diff_test(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "Saved server default absent from prior authoring must remain unmanaged, got {result:?}"
    );
}

#[test]
fn list_union_authoring_does_not_turn_provider_field_into_removal() {
    let resource_id = ResourceId::with_identity("example.Listener", "listener");
    let desired_rules = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("web".into())),
            ),
        ]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(443)),
        )]))),
    ]));
    let current_rules = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(80))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("web".into())),
            ),
        ]))),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("port".to_string(), Value::Concrete(ConcreteValue::Int(443))),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("provider-default".into())),
            ),
        ]))),
    ]));
    let desired =
        Resource::new("example.Listener", "listener").with_attribute("rules", desired_rules);
    let current_attrs = HashMap::from([("rules".to_string(), current_rules)]);
    let current = existing_state(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::List {
                element: Box::new(ExplicitFields::Struct {
                    children: HashMap::from([
                        ("port".to_string(), ExplicitFields::Leaf),
                        ("description".to_string(), ExplicitFields::Leaf),
                    ]),
                }),
            },
        )]),
    };

    let result = diff_test(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "List-union authoring must not unset a field populated on only one element, got {result:?}"
    );
}

#[test]
fn list_elements_authored_field_removal_produces_update_for_paired_element() {
    let resource_id = ResourceId::with_identity("example.Listener", "listener");
    let rule = |port, description: Option<&str>| {
        let mut fields = IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(port)),
        )]);
        if let Some(description) = description {
            fields.insert(
                "description".to_string(),
                Value::Concrete(ConcreteValue::String(description.to_string().into())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let desired_rules = Value::Concrete(ConcreteValue::List(vec![rule(80, None), rule(443, None)]));
    let current_rules = Value::Concrete(ConcreteValue::List(vec![
        rule(443, Some("provider-default")),
        rule(80, Some("web")),
    ]));
    let desired =
        Resource::new("example.Listener", "listener").with_attribute("rules", desired_rules);
    let current_attrs = HashMap::from([("rules".to_string(), current_rules)]);
    let current = existing_state(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::ListElements {
                elements: vec![
                    ExplicitFields::Struct {
                        children: HashMap::from([("port".to_string(), ExplicitFields::Leaf)]),
                    },
                    ExplicitFields::Struct {
                        children: HashMap::from([
                            ("port".to_string(), ExplicitFields::Leaf),
                            ("description".to_string(), ExplicitFields::Leaf),
                        ]),
                    },
                ],
            },
        )]),
    };

    let ExplicitFields::Struct { children } = &prev_explicit else {
        unreachable!();
    };
    let effective_desired = crate::resource::merge_with_saved(
        &desired.attributes["rules"],
        crate::resource::SavedValueViews::same(&current_attrs["rules"]),
        &children["rules"],
        None,
        crate::schema::empty_defs_for_schema_walks(),
    );
    let Value::Concrete(ConcreteValue::List(effective_rules)) = effective_desired else {
        panic!("expected effective desired rules list");
    };
    let Value::Concrete(ConcreteValue::Map(port_80)) = &effective_rules[0] else {
        panic!("expected port 80 map");
    };
    let Value::Concrete(ConcreteValue::Map(port_443)) = &effective_rules[1] else {
        panic!("expected port 443 map");
    };
    assert!(!port_80.contains_key("description"));
    assert_eq!(
        port_443.get("description"),
        Some(&Value::Concrete(ConcreteValue::String(
            "provider-default".into()
        )))
    );

    let result = diff_test(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    let Diff::Update {
        changed_attributes, ..
    } = result
    else {
        panic!("Expected the paired authored-field removal to produce an Update");
    };
    assert_eq!(changed_attributes, vec!["rules".to_string()]);
}

#[test]
fn legacy_migrated_leaf_list_authoring_preserves_provider_nested_fields() {
    let resource_id = ResourceId::with_identity("example.Listener", "listener");
    let rule = |description: Option<&str>| {
        let mut fields =
            IndexMap::from([("port".to_string(), Value::Concrete(ConcreteValue::Int(80)))]);
        if let Some(description) = description {
            fields.insert(
                "description".to_string(),
                Value::Concrete(ConcreteValue::String(description.to_string().into())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let desired_rules = Value::Concrete(ConcreteValue::List(vec![rule(None), rule(None)]));
    let current_rules = Value::Concrete(ConcreteValue::List(vec![rule(Some("web")), rule(None)]));
    let desired = Resource::new("example.Listener", "listener")
        .with_attribute("rules", desired_rules.clone());
    let current_attrs = HashMap::from([("rules".to_string(), current_rules)]);
    let current = existing_state(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([("rules".to_string(), ExplicitFields::Leaf)]),
    };

    let result = diff_test(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );
    assert!(
        matches!(result, Diff::NoChange(_)),
        "legacy Leaf list authoring must retain provider-populated element fields, got {result:?}"
    );
}

#[test]
fn whole_value_list_authoring_compares_and_sends_the_full_desired_list() {
    let resource_id = ResourceId::with_identity("example.Listener", "listener");
    let rule = |description: Option<&str>| {
        let mut fields =
            IndexMap::from([("port".to_string(), Value::Concrete(ConcreteValue::Int(80)))]);
        if let Some(description) = description {
            fields.insert(
                "description".to_string(),
                Value::Concrete(ConcreteValue::String(description.to_string().into())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let desired_rules = Value::Concrete(ConcreteValue::List(vec![rule(None), rule(None)]));
    let current_rules = Value::Concrete(ConcreteValue::List(vec![rule(Some("web")), rule(None)]));
    let desired = Resource::new("example.Listener", "listener")
        .with_attribute("rules", desired_rules.clone());
    let current_attrs = HashMap::from([("rules".to_string(), current_rules)]);
    let current = existing_state(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::WholeValue {
                pending: PendingPaths::try_new([vec!["description".to_string()]]).unwrap(),
            },
        )]),
    };

    let result = diff_test(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );
    let Diff::Update {
        from,
        to,
        changed_attributes,
        ..
    } = result
    else {
        panic!("WholeValue list authoring must compare the full list value");
    };
    assert_eq!(changed_attributes, vec!["rules".to_string()]);

    let schema = crate::schema::ResourceSchema::new("example.Listener");
    let patch = crate::provider::build_update_patch(
        &changed_attributes,
        &provider_ready(to),
        &from,
        &schema,
    )
    .expect("fixture update patch is valid");
    assert_eq!(patch.ops.len(), 1);
    assert!(matches!(
        &patch.ops[0],
        crate::provider::PatchOp::Replace { value, .. } if value == &desired_rules
    ));
}

#[test]
fn unchanged_dsl_after_writeback_pairs_raw_saved_list_without_phantom_removal() {
    let cfg = |with_timeout: bool| {
        let mut fields = IndexMap::from([(
            "retries".to_string(),
            Value::Concrete(ConcreteValue::Int(3)),
        )]);
        if with_timeout {
            fields.insert(
                "timeout".to_string(),
                Value::Concrete(ConcreteValue::Int(30)),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let rule = |id: &str, with_description: bool, with_timeout: bool| {
        let mut fields = IndexMap::from([
            (
                "id".to_string(),
                Value::Concrete(ConcreteValue::String(id.to_string().into())),
            ),
            ("cfg".to_string(), cfg(with_timeout)),
        ]);
        if with_description {
            fields.insert(
                "desc".to_string(),
                Value::Concrete(ConcreteValue::String(String::new().into())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };

    let authored = Resource::new("example.Listener", "listener").with_attribute(
        "rules",
        Value::Concrete(ConcreteValue::List(vec![
            rule("first", false, false),
            rule("second", true, false),
        ])),
    );
    let saved_attributes = HashMap::from([(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            rule("first", false, true),
            rule("second", true, true),
            rule("server-default", true, true),
        ])),
    )]);
    let sent = crate::executor::AcceptedRequest::create_for_test();
    let sent_desired = sent.sent_desired_attributes(&authored);
    let prior = crate::explicit::build_from_resource_for_stored_values_after_send(
        &authored,
        None,
        &saved_attributes,
        &sent_desired,
        None,
    );
    let ExplicitFields::Struct { children } = &prior else {
        panic!("expected resource-root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected stored-index-aligned ListElements");
    };
    assert_eq!(elements.len(), 3);
    assert!(matches!(elements[2], ExplicitFields::Unrecorded));

    let current = existing_state(authored.id.clone(), saved_attributes.clone());
    let result = diff_test(
        &authored,
        &current,
        Some(&saved_attributes),
        Some(&prior),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "unchanged DSL after aligned writeback must not remove a sibling provider default, got {result:?}"
    );
}

#[test]
fn migrated_leaf_authoring_for_map_preserves_provider_nested_fields() {
    let resource_id = ResourceId::with_identity("example.Service", "service");
    let desired_settings = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "mode".to_string(),
        Value::Concrete(ConcreteValue::String("active".into())),
    )])));
    let current_settings = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "mode".to_string(),
            Value::Concrete(ConcreteValue::String("active".into())),
        ),
        (
            "provider_default".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
    ])));
    let desired =
        Resource::new("example.Service", "service").with_attribute("settings", desired_settings);
    let current_attrs = HashMap::from([("settings".to_string(), current_settings)]);
    let current = existing_state(resource_id, current_attrs.clone());
    let prev_explicit = ExplicitFields::Struct {
        children: HashMap::from([("settings".to_string(), ExplicitFields::Leaf)]),
    };

    let result = diff_test(
        &desired,
        &current,
        Some(&current_attrs),
        Some(&prev_explicit),
        None,
    );

    assert!(
        matches!(result, Diff::NoChange(_)),
        "Migrated Leaf authoring must retain nested provider fields, got {result:?}"
    );
}

#[test]
fn nested_reference_removal_updates_consumer_before_deleting_orphan() {
    let distribution_id =
        ResourceId::with_identity("awscc.cloudfront.Distribution", "distribution");
    let web_acl_id = ResourceId::with_identity("awscc.wafv2.WebACL", "web-acl");
    let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    )])));
    let desired = Resource::new("awscc.cloudfront.Distribution", "distribution")
        .with_binding("distribution")
        .with_attribute("distribution_config", desired_config);

    let current_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "enabled".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
        (
            "web_acl_id".to_string(),
            Value::Concrete(ConcreteValue::String("arn:aws:wafv2:acl/test".into())),
        ),
    ])));
    let distribution_attrs = HashMap::from([("distribution_config".to_string(), current_config)]);
    let distribution_state = existing_state(distribution_id.clone(), distribution_attrs.clone())
        .with_dependency_bindings(BTreeSet::from(["web_acl".to_string()]));
    let web_acl_state = existing_state(web_acl_id.clone(), HashMap::new())
        .with_identifier("arn:aws:wafv2:acl/test");
    let current_states = HashMap::from([(distribution_id.clone(), distribution_state)]);
    let saved_attrs = crate::provider::RawSavedAttrs::from_decoded_values_for_test(HashMap::from(
        [(distribution_id.clone(), distribution_attrs)],
    ))
    .lift(&SchemaRegistry::new());
    let prev_explicit = HashMap::from([(
        distribution_id,
        ExplicitFields::Struct {
            children: HashMap::from([(
                "distribution_config".to_string(),
                ExplicitFields::Struct {
                    children: HashMap::from([
                        ("enabled".to_string(), ExplicitFields::Leaf),
                        ("web_acl_id".to_string(), ExplicitFields::Leaf),
                    ]),
                },
            )]),
        },
    )]);
    let orphans = Orphans::from([Orphan::from_state(
        web_acl_state,
        Some("web_acl".to_string()),
        Directives::default(),
    )]);

    let plan = create_plan(
        &[desired],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &saved_attrs,
        &prev_explicit,
        &orphans,
        &[],
    );

    let update = plan
        .effects()
        .iter()
        .find(|effect| matches!(effect, Effect::Update { .. }))
        .expect("Distribution must be updated");
    let Effect::Update {
        from,
        identifier: _identifier,
        to,
        changed_attributes,
    } = update
    else {
        unreachable!();
    };
    assert_eq!(changed_attributes, &["distribution_config".to_string()]);
    let ready = provider_ready(to.as_resource().clone());
    let schema = crate::schema::ResourceSchema::new("awscc.cloudfront.Distribution");
    let patch = crate::provider::build_update_patch(changed_attributes, &ready, from, &schema)
        .expect("fixture update patch is valid");
    let crate::provider::PatchOp::Replace {
        value: Value::Concrete(ConcreteValue::Map(patch_config)),
        ..
    } = &patch.ops[0]
    else {
        panic!("Expected distribution_config replacement value");
    };
    assert!(!patch_config.contains_key("web_acl_id"));

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &web_acl_id)
        .expect("WebACL must be deleted");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        panic!("Expected orphan WebACL Delete");
    };
    assert_eq!(
        blocked_by_updates,
        &HashSet::from([ResourceIdentity::new("distribution")])
    );
}

#[test]
fn orphan_delete_uses_typed_binding_without_state_attribute() {
    let orphan_id = ResourceId::with_identity("example.Service", "removed-service");
    let orphan_state = existing_state(
        orphan_id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("removed-service".into())),
        )]),
    )
    .with_identifier("service-123")
    .with_dependency_bindings(BTreeSet::from(["network".to_string()]));
    assert!(
        orphan_state
            .attributes
            .keys()
            .all(|key| !key.starts_with('_')),
        "provider-facing state must not carry orphan metadata"
    );

    let orphans = Orphans::from([Orphan::from_state(
        orphan_state,
        Some("service".to_string()),
        Directives::default(),
    )]);

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            HashMap::new(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &orphan_id)
        .expect("orphan must produce a Delete effect");
    let Effect::Delete {
        binding,
        dependencies,
        ..
    } = delete
    else {
        panic!("expected orphan Delete, got {delete:?}");
    };
    assert_eq!(binding.as_deref(), Some("service"));
    assert_eq!(dependencies, &HashSet::from(["network".to_string()]));
}

#[test]
fn desired_resource_uses_same_id_orphan_as_its_current_state_without_explicit_promotion() {
    let id = ResourceId::with_identity("example.Service", "service");
    let desired = Resource::new("example.Service", "service")
        .with_attribute("name", Value::Concrete(ConcreteValue::String("new".into())));
    let orphan = Orphan::from_state(
        existing_state(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("old".into())),
            )]),
        )
        .with_identifier("service-123"),
        Some("service".to_string()),
        Directives::default(),
    );

    let plan = create_plan(
        &[desired],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            HashMap::new(),
            &SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::from([orphan]),
        &[],
    );

    assert_eq!(plan.effects().len(), 1, "plan was {plan:#?}");
    let Effect::Update {
        from,
        changed_attributes,
        ..
    } = &plan.effects()[0]
    else {
        panic!("expected an Update against the orphan state, got {plan:#?}");
    };
    assert_eq!(from.identifier.as_deref(), Some("service-123"));
    assert_eq!(changed_attributes, &["name".to_string()]);
}

#[test]
fn orphan_collection_rekeys_index_and_provider_state_together() {
    let old_id = ResourceId::with_identity("example.Service", "old");
    let new_id = ResourceId::with_identity("example.Service", "new");
    let orphan = Orphan::from_state(
        existing_state(old_id.clone(), HashMap::new()),
        Some("service".to_string()),
        Directives::default(),
    );
    let mut orphans: Orphans = [orphan].into_iter().collect();

    let move_effect: Effect = Effect::Move {
        from: ResolvedResourceId::new(old_id.clone()),
        to: ResolvedResourceId::new(new_id.clone()),
        origin: None,
    };
    let authority = move_effect
        .move_authority()
        .expect("the fixture effect is a move");
    assert!(orphans.rekey(&authority));
    assert!(!orphans.contains_key(&old_id));
    let moved = match orphans.get(&new_id).unwrap().current() {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("test fixture cannot be in flight"),
        Current::MissingIdentifier(state) => state.state(),
    };
    assert_eq!(moved.id, new_id);
}

#[tokio::test]
async fn prior_state_transform_keeps_managed_and_data_source_key_spaces_separate() {
    let id = ResourceId::with_provider_identity("mock", "test.Lookup", "shared", None);
    let mut current_states = crate::resource::current_map_from_states(HashMap::from([(
        id.clone(),
        existing_state(id.clone(), HashMap::new()),
    )]));
    let mut data_source_states =
        HashMap::from([(id.clone(), State::existing(id.clone(), HashMap::new()))]);
    let mut orphans = Orphans::new();

    transform_prior_provider_states(
        &mut current_states,
        &mut data_source_states,
        &mut orphans,
        async |states| {
            for state in states.values_mut() {
                state.attributes.insert(
                    "managed_normalized".to_string(),
                    Value::Concrete(ConcreteValue::Bool(true)),
                );
            }
            Ok::<(), crate::provider::ProviderError>(())
        },
        async |states| {
            for state in states.values_mut() {
                state.attributes.insert(
                    "data_source_normalized".to_string(),
                    Value::Concrete(ConcreteValue::Bool(true)),
                );
            }
            Ok::<(), crate::provider::ProviderError>(())
        },
    )
    .await
    .expect("overlapping ids remain isolated across transform calls");

    let Current::Present(managed) = &current_states[&id] else {
        panic!("managed fixture must remain present");
    };
    assert_eq!(
        managed.state().attributes.get("managed_normalized"),
        Some(&Value::Concrete(ConcreteValue::Bool(true)))
    );
    assert!(
        !managed
            .state()
            .attributes
            .contains_key("data_source_normalized")
    );
    assert_eq!(
        data_source_states[&id]
            .attributes
            .get("data_source_normalized"),
        Some(&Value::Concrete(ConcreteValue::Bool(true)))
    );
    assert!(
        !data_source_states[&id]
            .attributes
            .contains_key("managed_normalized")
    );
}

#[tokio::test]
async fn prior_state_transform_rebuilds_managed_current_after_provider_transform() {
    let id = ResourceId::with_identity("example.Service", "current");
    let mut current_states = crate::resource::current_map_from_states(HashMap::from([(
        id.clone(),
        existing_state(id.clone(), HashMap::new()),
    )]));
    let mut orphans = Orphans::new();
    let mut data_source_states = HashMap::new();

    transform_prior_provider_states(
        &mut current_states,
        &mut data_source_states,
        &mut orphans,
        async |states| {
            states.get_mut(&id).unwrap().attributes.insert(
                "normalized".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            );
            Ok::<(), crate::provider::ProviderError>(())
        },
        async |states| {
            assert!(states.is_empty());
            Ok::<(), crate::provider::ProviderError>(())
        },
    )
    .await
    .unwrap();

    let current = current_states
        .get(&id)
        .expect("managed current must be restored after the transform");
    let state = match current {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("test fixture cannot be in flight"),
        Current::MissingIdentifier(state) => state.state(),
    };
    assert_eq!(
        state.attributes.get("normalized"),
        Some(&Value::Concrete(ConcreteValue::Bool(true)))
    );
}

#[tokio::test]
async fn prior_state_transform_preserves_typed_managed_identifier() {
    let id = ResourceId::with_identity("example.Service", "current");
    let state = State::existing(id.clone(), HashMap::new());
    let identifier = crate::provider::fixture_applied_identifier("typed-provider-id");
    let mut current_states = HashMap::from([(
        id.clone(),
        crate::resource::fixture_present_current(state, identifier),
    )]);
    let mut orphans = Orphans::new();
    let mut data_source_states = HashMap::new();

    transform_prior_provider_states(
        &mut current_states,
        &mut data_source_states,
        &mut orphans,
        async |states| {
            states.get_mut(&id).unwrap().attributes.insert(
                "normalized".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            );
            Ok::<(), crate::provider::ProviderError>(())
        },
        async |states| {
            assert!(states.is_empty());
            Ok::<(), crate::provider::ProviderError>(())
        },
    )
    .await
    .unwrap();

    let Current::Present(present) = &current_states[&id] else {
        panic!("normalization must retain the authoritative Present classification");
    };
    assert_eq!(present.identifier().as_str(), "typed-provider-id");
    assert_eq!(
        present.state().attributes.get("normalized"),
        Some(&Value::Concrete(ConcreteValue::Bool(true)))
    );
}

#[test]
fn orphan_delete_keeps_persisted_explicit_dependencies_without_binding_dependencies() {
    let orphan_id = ResourceId::with_identity("example.Service", "removed-service");
    let orphan_state =
        existing_state(orphan_id.clone(), HashMap::new()).with_identifier("service-123");
    let directives = Directives {
        depends_on: vec!["x".to_string()],
        ..Directives::default()
    };
    let orphans = Orphans::from([Orphan::from_state(
        orphan_state,
        Some("service".to_string()),
        directives,
    )]);

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            HashMap::new(),
            &SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &orphan_id)
        .expect("orphan must produce a Delete effect");
    let Effect::Delete {
        dependencies,
        explicit_dependencies,
        ..
    } = delete
    else {
        panic!("expected orphan Delete, got {delete:?}");
    };
    assert_eq!(dependencies, &HashSet::from(["x".to_string()]));
    assert_eq!(explicit_dependencies, &HashSet::from(["x".to_string()]));
}

#[test]
fn replaced_producer_delete_waits_for_consumer_that_dropped_prior_reference() {
    use crate::schema::{AttributeSchema, AttributeType};

    let producer_id = ResourceId::with_identity("example.Producer", "producer-id");
    let consumer_id = ResourceId::with_identity("example.Consumer", "consumer-id");
    let producer = Resource::new("example.Producer", "producer-id")
        .with_binding("producer")
        .with_attribute(
            "replace_key",
            Value::Concrete(ConcreteValue::String("new".into())),
        );
    let consumer = Resource::new("example.Consumer", "consumer-id").with_binding("consumer");
    let producer_state = existing_state(
        producer_id.clone(),
        HashMap::from([(
            "replace_key".to_string(),
            Value::Concrete(ConcreteValue::String("old".into())),
        )]),
    )
    .with_identifier("producer-old");
    let consumer_state = existing_state(
        consumer_id.clone(),
        HashMap::from([(
            "producer_id".to_string(),
            Value::Concrete(ConcreteValue::String("producer-old".into())),
        )]),
    )
    .with_dependency_bindings(BTreeSet::from(["producer".to_string()]));
    let current_states = HashMap::from([
        (producer_id.clone(), producer_state),
        (consumer_id.clone(), consumer_state),
    ]);
    let prev_explicit = HashMap::from([(consumer_id, explicit_top_level(&["producer_id"]))]);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("example.Producer").attribute(
            AttributeSchema::optional("replace_key", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &[producer, consumer],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &prev_explicit,
        &Orphans::new(),
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| matches!(effect, Effect::Delete { id, .. } if id.as_inner() == &producer_id))
        .expect("Replacement must contain the old producer Delete");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        unreachable!();
    };
    assert_eq!(
        blocked_by_updates,
        &HashSet::from([ResourceIdentity::new("consumer-id")])
    );
}

#[test]
fn prior_consumer_edge_is_not_added_when_desired_still_depends_on_delete_binding() {
    let dependency_id = ResourceId::with_identity("example.Dependency", "dependency-id");
    let consumer_id = ResourceId::with_identity("example.Consumer", "consumer-id");
    let consumer = Resource::new("example.Consumer", "consumer-id")
        .with_binding("consumer")
        .with_dependency_bindings(BTreeSet::from(["dependency".to_string()]))
        .with_attribute("version", Value::Concrete(ConcreteValue::Int(2)));
    let consumer_state = existing_state(
        consumer_id.clone(),
        HashMap::from([(
            "version".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        )]),
    )
    .with_dependency_bindings(BTreeSet::from(["dependency".to_string()]));
    let dependency_state =
        existing_state(dependency_id.clone(), HashMap::new()).with_identifier("dependency-old");
    let current_states = HashMap::from([(consumer_id, consumer_state)]);
    let orphans = Orphans::from([Orphan::from_state(
        dependency_state,
        Some("dependency".to_string()),
        Directives::default(),
    )]);

    let plan = create_plan(
        &[consumer],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &dependency_id)
        .expect("Orphan dependency must be deleted");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        panic!("Expected orphan Delete");
    };
    assert!(
        blocked_by_updates.is_empty(),
        "The inverse pass must not add an edge while the desired consumer still depends on the binding"
    );
}

#[test]
fn orphan_delete_waits_for_consumer_update_from_prior_depends_on() {
    let dependency_id = ResourceId::with_identity("example.Dependency", "dependency-id");
    let consumer_id = ResourceId::with_identity("example.Consumer", "consumer-id");
    let consumer = Resource::new("example.Consumer", "consumer-id")
        .with_binding("consumer")
        .with_attribute("version", Value::Concrete(ConcreteValue::Int(2)));
    let consumer_state = existing_state(
        consumer_id.clone(),
        HashMap::from([(
            "version".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        )]),
    );
    let dependency_state =
        existing_state(dependency_id.clone(), HashMap::new()).with_identifier("dependency-old");
    let current_states = HashMap::from([(consumer_id.clone(), consumer_state)]);
    let directives_map = HashMap::from([(
        consumer_id,
        Directives {
            depends_on: vec!["dependency".to_string()],
            ..Default::default()
        },
    )]);
    let orphans = Orphans::from([Orphan::from_state(
        dependency_state,
        Some("dependency".to_string()),
        Directives::default(),
    )]);

    let plan = create_plan(
        &[consumer],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| effect.resource_id() == &dependency_id)
        .expect("Orphan dependency must be deleted");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        panic!("Expected orphan Delete");
    };
    assert_eq!(
        blocked_by_updates,
        &HashSet::from([ResourceIdentity::new("consumer-id")])
    );
}

#[test]
fn explicit_top_level_removal_still_detected() {
    // Regression guard: removing the *whole* attribute (top-level key
    // gone from desired but still authored in prev_explicit) must
    // still produce an Update. This is the existing "explicit unset"
    // mechanism; the projection logic must not regress it.
    let desired = Resource::new("s3.Bucket", "test");

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Env".to_string(),
            Value::Concrete(ConcreteValue::String("prod".into())),
        )]))),
    );
    let current = existing_state(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

    let prev_explicit = explicit_top_level(&["tags"]);
    let result = diff_test(&desired, &current, None, Some(&prev_explicit), None);

    match result {
        Diff::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"tags".to_string()),
                "Removed top-level attr must still produce Update, got: {:?}",
                changed_attributes
            );
        }
        _ => panic!("Expected Update for explicit-unset, got {:?}", result),
    }
}

#[test]
fn diff_no_change_without_prev_desired_keys() {
    // Without prev_desired_keys, removed attributes should NOT be detected
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    current_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        )]))),
    );
    let current = existing_state(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

    let result = diff_test(&desired, &current, None, None, None);
    assert!(
        matches!(result, Diff::NoChange(_)),
        "Without prev_desired_keys, extra attributes in current should not trigger Update, got {:?}",
        result
    );
}

#[test]
fn create_plan_detects_attribute_removal() {
    // Resource in .crn has no "tags", but current state (from AWS) has tags.
    // prev_desired_keys indicates user previously had "region" and "tags".
    let resources = vec![Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        )]))),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        existing_state(ResourceId::with_identity("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        explicit_top_level(&["region", "tags"]),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &prev_explicit,
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Update { .. }),
        "Expected Update effect for attribute removal, got {:?}",
        plan.effects()[0]
    );
}

#[test]
fn create_plan_filters_non_removable_attribute_removal() {
    use crate::schema::{AttributeSchema, AttributeType};
    // When schema is available, only removable attributes should trigger removal.
    // "region" is not removable, "tags" is removable.
    let resources = vec![Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        )]))),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        existing_state(ResourceId::with_identity("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        explicit_top_level(&["region", "tags"]),
    );

    // Schema: tags is auto-removable (optional, not create-only),
    // region is explicitly non-removable (provider-inherited)
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("s3.Bucket")
            .attribute(
                AttributeSchema::optional("region", AttributeType::string())
                    .non_removable()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &prev_explicit,
        &Orphans::new(),
        &[],
    );

    assert_eq!(plan.effects().len(), 1);
    match &plan.effects()[0] {
        Effect::Update {
            changed_attributes, ..
        } => {
            assert!(
                changed_attributes.contains(&"tags".to_string()),
                "Should detect removable 'tags' removal"
            );
            assert!(
                !changed_attributes.contains(&"region".to_string()),
                "Should NOT detect non-removable 'region' removal"
            );
        }
        _ => panic!("Expected Update effect"),
    }
}

#[test]
fn create_plan_skips_update_when_only_non_removable_removal() {
    use crate::schema::{AttributeSchema, AttributeType};
    // When the only "change" is a non-removable attribute removal,
    // the plan should have no effects (no spurious Update).
    let resources = vec![Resource::new("s3.Bucket", "test").with_attribute(
        "bucket",
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    )];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        existing_state(ResourceId::with_identity("s3.Bucket", "test"), attrs),
    );

    let mut prev_explicit = HashMap::new();
    prev_explicit.insert(
        ResourceId::with_identity("s3.Bucket", "test"),
        explicit_top_level(&["bucket", "region"]),
    );

    // Schema: region is explicitly non-removable, bucket is required
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::required("bucket", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("region", AttributeType::string())
                    .non_removable()
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &prev_explicit,
        &Orphans::new(),
        &[],
    );

    assert!(
        plan.effects().is_empty(),
        "Should not generate spurious Update for non-removable attribute removal, got {:?}",
        plan.effects()
    );
}

#[test]
fn diff_includes_underscore_prefixed_attributes_in_removal_detection() {
    let desired = Resource::new("s3.Bucket", "test").with_attribute(
        "region",
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );

    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    current_attrs.insert(
        "_internal".to_string(),
        Value::Concrete(ConcreteValue::String("something".into())),
    );
    let current = existing_state(
        ResourceId::with_identity("s3.Bucket", "test"),
        current_attrs,
    );

    let prev_explicit = explicit_top_level(&["region", "_internal"]);

    let result = diff_test(&desired, &current, None, Some(&prev_explicit), None);
    let Diff::Update {
        changed_attributes, ..
    } = result
    else {
        panic!("underscore-prefixed provider attribute removal must produce Update");
    };
    assert_eq!(changed_attributes, vec!["_internal".to_string()]);
}

#[test]
fn prevent_destroy_blocks_delete_for_orphaned_resource() {
    // Orphaned resource (in state but removed from .crn) with prevent_destroy
    // should produce a PlanError instead of a Delete effect.

    let orphan_id = ResourceId::with_identity("ec2.Vpc", "my-vpc");
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    let orphans = Orphans::from([Orphan::from_state(
        existing_state(orphan_id.clone(), attrs),
        None,
        Directives {
            prevent_destroy: true,
            ..Default::default()
        },
    )]);

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            HashMap::new(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    // Should have NO delete effects
    assert!(
        plan.effects().is_empty(),
        "Should not generate Delete effect for prevent_destroy resource, got {:?}",
        plan.effects()
    );

    // Should have an error
    assert!(plan.has_errors(), "Should have prevent_destroy error");
    assert_eq!(plan.errors().len(), 1);
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::PreventDestroy {
            action: PreventDestroyAction::DeleteRemovedFromConfiguration
        }
    );
    assert_eq!(
        plan.errors()[0].resource_id,
        ResourceId::with_identity("ec2.Vpc", "my-vpc")
    );
}

#[test]
fn prevent_destroy_blocks_replace() {
    use crate::schema::{AttributeSchema, AttributeType};

    // Resource with prevent_destroy that has a create-only attribute change
    // (which would normally trigger a Replace)
    let mut resource = Resource::new("ec2.Vpc", "my-vpc").with_attribute(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
    );
    resource.directives.prevent_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    current_states.insert(
        ResourceId::with_identity("ec2.Vpc", "my-vpc"),
        existing_state(ResourceId::with_identity("ec2.Vpc", "my-vpc"), attrs),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Vpc").attribute(
            AttributeSchema::optional("cidr_block", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    // Should have NO replace effects
    assert!(
        plan.effects().is_empty(),
        "Should not generate Replace effect for prevent_destroy resource, got {:?}",
        plan.effects()
    );

    // Should have an error
    assert!(
        plan.has_errors(),
        "Should have prevent_destroy error for replace"
    );
    assert_eq!(plan.errors().len(), 1);
    assert_eq!(
        &plan.errors()[0].kind,
        &PlanErrorKind::PreventDestroy {
            action: PreventDestroyAction::Replace
        }
    );
}

#[test]
fn prevent_destroy_does_not_block_update() {
    // Resource with prevent_destroy that has a normal (non-create-only) attribute change
    // Updates don't destroy the resource, so they should be allowed
    let mut resource = Resource::new("s3.Bucket", "my-bucket").with_attribute(
        "versioning",
        Value::Concrete(ConcreteValue::String("Enabled".into())),
    );
    resource.directives.prevent_destroy = true;

    let resources = vec![resource];

    let mut current_states = HashMap::new();
    let mut attrs = HashMap::new();
    attrs.insert(
        "versioning".to_string(),
        Value::Concrete(ConcreteValue::String("Disabled".into())),
    );
    current_states.insert(
        ResourceId::with_identity("s3.Bucket", "my-bucket"),
        existing_state(ResourceId::with_identity("s3.Bucket", "my-bucket"), attrs),
    );

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    // Should generate an Update effect (not blocked)
    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Update { .. }),
        "Expected Update effect, got {:?}",
        plan.effects()[0]
    );

    // Should have NO errors
    assert!(
        !plan.has_errors(),
        "Should not have errors for Update with prevent_destroy"
    );
}

#[test]
fn prevent_destroy_does_not_block_create() {
    // Resource with prevent_destroy that doesn't exist yet
    // Creates don't destroy anything, so they should be allowed
    let mut resource = Resource::new("s3.Bucket", "my-bucket").with_attribute(
        "bucket",
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    resource.directives.prevent_destroy = true;

    let resources = vec![resource];

    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(), // no current states (resource doesn't exist)
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    // Should generate a Create effect (not blocked)
    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Create(_)),
        "Expected Create effect, got {:?}",
        plan.effects()[0]
    );

    // Should have NO errors
    assert!(
        !plan.has_errors(),
        "Should not have errors for Create with prevent_destroy"
    );
}

#[test]
fn route53_union_non_enum_literal_reaches_plan_as_string() {
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
    };

    let hosted_zone = AttributeType::enum_(
        TypeIdentity::new(
            Some("aws"),
            ["route53", "RecordSet", "AliasTarget"],
            "HostedZoneId",
        ),
        Some(vec!["Z2FDTNDATAQYW2".to_string()]),
        Vec::new(),
        None,
        None,
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (ResourceSchema::new("route53.RecordSet").attribute(
            AttributeSchema::optional(
                "alias_target",
                AttributeType::struct_(
                    "AliasTarget",
                    vec![
                        StructField::optional(
                            "hosted_zone_id",
                            AttributeType::union(vec![hosted_zone, AttributeType::string()]),
                        )
                        .build(),
                    ],
                ),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let literal = Value::Concrete(ConcreteValue::String("Z05136711ZXUHBDOA8D5O".into()));
    let mut resources = vec![
        Resource::with_provider("aws", "route53.RecordSet", "record", None).with_attribute(
            "alias_target",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "hosted_zone_id".to_string(),
                literal.clone(),
            )]))),
        ),
    ];

    crate::value::canonicalize_resources_with_schemas(&mut resources, &schemas)
        .expect("the literal String arm must be selected before planning");
    let plan = create_plan(
        &resources,
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[],
    );

    assert!(
        !plan.has_errors(),
        "unexpected plan errors: {:?}",
        plan.errors()
    );
    let Effect::Create(resource) = &plan.effects()[0] else {
        panic!("expected a Create effect, got {:?}", plan.effects()[0]);
    };
    let Some(Value::Concrete(ConcreteValue::Map(alias_target))) = resource.get_attr("alias_target")
    else {
        panic!("expected alias_target map in planned resource");
    };
    assert_eq!(alias_target.get("hosted_zone_id"), Some(&literal));
}

#[test]
fn without_prevent_destroy_delete_works_normally() {
    // Orphaned resource without prevent_destroy should still be deleted normally
    let orphan_id = ResourceId::with_identity("ec2.Vpc", "my-vpc");
    let mut attrs = HashMap::new();
    attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    let orphans = Orphans::from([Orphan::from_state(
        existing_state(orphan_id, attrs),
        None,
        Directives::default(),
    )]);

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            HashMap::new(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(), // no directives (default = prevent_destroy: false)
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    // Should generate a Delete effect
    assert_eq!(plan.effects().len(), 1);
    assert!(
        matches!(&plan.effects()[0], Effect::Delete { .. }),
        "Expected Delete effect, got {:?}",
        plan.effects()[0]
    );

    // Should have NO errors
    assert!(!plan.has_errors());
}

#[test]
fn prevent_destroy_collects_multiple_errors() {
    // Two orphaned resources with prevent_destroy - both should generate errors
    let first_id = ResourceId::with_identity("ec2.Vpc", "vpc-1");
    let second_id = ResourceId::with_identity("ec2.Vpc", "vpc-2");
    let mut attrs1 = HashMap::new();
    attrs1.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    let mut attrs2 = HashMap::new();
    attrs2.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
    );
    let prevent_destroy = Directives {
        prevent_destroy: true,
        ..Default::default()
    };
    let orphans = Orphans::from([
        Orphan::from_state(
            existing_state(first_id, attrs1),
            None,
            prevent_destroy.clone(),
        ),
        Orphan::from_state(existing_state(second_id, attrs2), None, prevent_destroy),
    ]);

    let plan = create_plan(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &crate::resource::into_plan_input_current_map_from_states(
            HashMap::new(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );

    assert!(plan.effects().is_empty());
    assert_eq!(
        plan.errors().len(),
        2,
        "Should collect errors for both resources"
    );
}

// carina#3181: the former `virtual_resources_are_skipped_in_plan` test
// is obsolete — `create_plan` now takes a `&[Resource]` /
// `&[DataSource]` pair, so a `LiveComposition` cannot be passed into the
// plan input at all. The "compositions produce no effect" invariant is now
// enforced by the type system rather than a runtime skip.

#[test]
fn wait_binding_lowers_to_wait_effect() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // A downstream consumer that references the wait binding and is
    // itself a pending change (Create) — carina#3101: the wait is
    // emitted only when it gates a real downstream change.
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, consumer];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == 'ready'".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ready".into())),
        },
        timeout_secs: Some(75 * 60),
        depends_on: vec![],
        line: 1,
    };

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (crate::schema::ResourceSchema::new("acm.Certificate").attribute(
            crate::schema::AttributeSchema::optional(
                "status",
                crate::schema::AttributeType::string(),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    let wait_effect = plan
        .effects()
        .iter()
        .find(|e| e.is_wait())
        .expect("expected Wait effect");
    let Effect::Wait {
        identity,
        target_id,
        until,
        until_surface,
        timeout,
        ..
    } = wait_effect
    else {
        unreachable!();
    };
    assert_eq!(identity.as_str(), "cert_issued");
    assert_eq!(target_id.id().identity_str(), "cert");
    assert_eq!(target_id.id().resource_type(), "acm.Certificate");
    assert_eq!(
        until,
        &WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("ready".into())),
        )
        .unwrap()
    );
    assert_eq!(*timeout, std::time::Duration::from_secs(75 * 60));
    assert_eq!(until_surface, "cert.status == 'ready'");
}

#[test]
fn unresolved_wait_enum_identifier_is_a_plan_error() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let job = Resource::new("job.Job", "job").with_binding("job");
    let mut consumer = Resource::new("job.Consumer", "consumer").with_binding("consumer");
    consumer.dependency_bindings.insert("job_ready".to_string());
    let wait = WaitBinding {
        binding: "job_ready".into(),
        target: "job".into(),
        until_raw: "job.state == ready".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["job".to_string(), "state".to_string()],
            rhs: Value::Concrete(ConcreteValue::enum_identifier("ready")),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (crate::schema::ResourceSchema::new("job.Job").attribute(
            crate::schema::AttributeSchema::optional(
                "state",
                crate::schema::AttributeType::string(),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let plan = create_plan(
        &[job, consumer],
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(plan.effects().iter().all(|effect| !effect.is_wait()));
    assert_eq!(plan.errors().len(), 1);
    assert_eq!(
        plan.errors()[0].to_string(),
        "job.Job.job: wait `job_ready`: cannot resolve `ready` as an enum value of `job.state`; quote it to compare as a string"
    );
}

#[test]
fn nested_wait_enum_resolves_and_matches_provider_state() {
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::schema::{AttributeSchema, ResourceSchema, StructField, TypeIdentity};

    let cert = Resource::with_provider("aws", "acm.Certificate", "cert", None).with_binding("cert");
    let mut consumer =
        Resource::with_provider("aws", "consumer.Thing", "consumer", None).with_binding("consumer");
    consumer
        .dependency_bindings
        .insert("cert_renewed".to_string());
    let wait = WaitBinding {
        binding: "cert_renewed".into(),
        target: "cert".into(),
        until_raw:
            "cert.renewal_summary.renewal_status == aws.acm.Certificate.RenewalStatus.success"
                .to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec![
                "cert".to_string(),
                "renewal_summary".to_string(),
                "renewal_status".to_string(),
            ],
            rhs: Value::Concrete(ConcreteValue::enum_identifier(
                "aws.acm.Certificate.RenewalStatus.success",
            )),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };
    let renewal_status = AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "RenewalStatus"),
        Some(vec!["SUCCESS".to_string()]),
        vec![("SUCCESS".to_string(), "success".to_string())],
        None,
        None,
    );
    let renewal_summary = AttributeType::struct_(
        "RenewalSummary",
        vec![StructField::optional("renewal_status", renewal_status).build()],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::optional("renewal_summary", renewal_summary).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &[cert, consumer],
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        plan.errors().is_empty(),
        "unexpected errors: {:?}",
        plan.errors()
    );
    let Effect::Wait { until, .. } = plan
        .effects()
        .iter()
        .find(|effect| effect.is_wait())
        .expect("nested wait should be emitted")
    else {
        unreachable!();
    };
    assert!(matches!(
        until.value(),
        Value::Concrete(ConcreteValue::CanonicalEnum(value))
            if value.api_value() == "SUCCESS"
    ));
    let observed = HashMap::from([(
        "renewal_summary".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "renewal_status".to_string(),
            Value::Concrete(ConcreteValue::String("SUCCESS".into())),
        )]))),
    )]);
    assert!(until.evaluate(&observed));
}

#[test]
fn quoted_wait_value_traverses_a_string_map_when_planning() {
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::schema::{AttributeSchema, ResourceSchema};

    let job = Resource::with_provider("test", "job.Job", "job", None).with_binding("job");
    let mut consumer = Resource::with_provider("test", "consumer.Thing", "consumer", None)
        .with_binding("consumer");
    consumer.dependency_bindings.insert("job_ready".to_string());
    let wait = WaitBinding {
        binding: "job_ready".into(),
        target: "job".into(),
        until_raw: "job.labels.state == 'ready'".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["job".into(), "labels".into(), "state".into()],
            rhs: Value::Concrete(ConcreteValue::String("ready".into())),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("job.Job").attribute(
            AttributeSchema::optional("labels", AttributeType::map(AttributeType::string()))
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &[job, consumer],
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        plan.errors().is_empty(),
        "unexpected errors: {:?}",
        plan.errors()
    );
    let Effect::Wait { until, .. } = plan
        .effects()
        .iter()
        .find(|effect| effect.is_wait())
        .expect("map-backed wait should be emitted")
    else {
        unreachable!();
    };
    let observed = HashMap::from([(
        "labels".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "state".to_string(),
            Value::Concrete(ConcreteValue::String("ready".into())),
        )]))),
    )]);
    assert!(until.evaluate(&observed));
}

#[test]
fn typed_enum_wait_value_traverses_an_enum_map_when_planning() {
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::schema::{AttributeSchema, ResourceSchema, TypeIdentity};

    let job = Resource::with_provider("test", "job.Job", "job", None).with_binding("job");
    let mut consumer = Resource::with_provider("test", "consumer.Thing", "consumer", None)
        .with_binding("consumer");
    consumer.dependency_bindings.insert("job_ready".to_string());
    let wait = WaitBinding {
        binding: "job_ready".into(),
        target: "job".into(),
        until_raw: "job.statuses.primary == test.job.Job.Status.ready".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["job".into(), "statuses".into(), "primary".into()],
            rhs: Value::Concrete(ConcreteValue::enum_identifier("test.job.Job.Status.ready")),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };
    let status = AttributeType::enum_(
        TypeIdentity::new(Some("test"), ["job", "Job"], "Status"),
        Some(vec!["READY".to_string()]),
        vec![("READY".to_string(), "ready".to_string())],
        None,
        None,
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("job.Job")
            .attribute(AttributeSchema::optional("statuses", AttributeType::map(status)).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &[job, consumer],
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        plan.errors().is_empty(),
        "unexpected errors: {:?}",
        plan.errors()
    );
    let Effect::Wait { until, .. } = plan
        .effects()
        .iter()
        .find(|effect| effect.is_wait())
        .expect("map-backed enum wait should be emitted")
    else {
        unreachable!();
    };
    assert!(matches!(
        until.value(),
        Value::Concrete(ConcreteValue::CanonicalEnum(value))
            if value.api_value() == "READY"
    ));
    let observed = HashMap::from([(
        "statuses".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "primary".to_string(),
            Value::Concrete(ConcreteValue::String("READY".into())),
        )]))),
    )]);
    assert!(until.evaluate(&observed));
}

#[test]
fn nested_wait_resolution_error_matches_validation_message() {
    use crate::parser::{ProviderContext, UntilPredicateAst, WaitBinding, parse};
    use crate::schema::{AttributeSchema, ResourceSchema, StructField, TypeIdentity};

    let renewal_status = AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "RenewalStatus"),
        Some(vec!["SUCCESS".to_string()]),
        vec![("SUCCESS".to_string(), "success".to_string())],
        None,
        None,
    );
    let renewal_summary = AttributeType::struct_(
        "RenewalSummary",
        vec![StructField::optional("renewal_status", renewal_status).build()],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::optional("renewal_summary", renewal_summary).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let source = r#"
let cert = aws.acm.Certificate {}
let renewed = wait cert {
    until = cert.renewal_summary.renewal_status == aws.acm.Certificate.RenewalStatus.bogus
}
"#;
    let parsed = parse(source, &ProviderContext::default()).expect("fixture must parse");
    let validation = crate::validation::wait::validate_wait_bindings(&parsed, &schemas);
    assert_eq!(validation.len(), 1);

    let cert = Resource::with_provider("aws", "acm.Certificate", "cert", None).with_binding("cert");
    let mut consumer =
        Resource::with_provider("aws", "consumer.Thing", "consumer", None).with_binding("consumer");
    consumer.dependency_bindings.insert("renewed".to_string());
    let wait = WaitBinding {
        binding: "renewed".into(),
        target: "cert".into(),
        until_raw: "cert.renewal_summary.renewal_status == aws.acm.Certificate.RenewalStatus.bogus"
            .to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec![
                "cert".to_string(),
                "renewal_summary".to_string(),
                "renewal_status".to_string(),
            ],
            rhs: Value::Concrete(ConcreteValue::enum_identifier(
                "aws.acm.Certificate.RenewalStatus.bogus",
            )),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };
    let plan = create_plan(
        &[cert, consumer],
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert_eq!(plan.errors().len(), 1);
    assert_eq!(plan.errors()[0].kind.to_string(), validation[0].message);
    assert!(
        validation[0]
            .message
            .contains("cert.renewal_summary.renewal_status")
    );
}

#[test]
fn data_source_wait_is_a_plan_error_and_emits_no_wait_effect() {
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::resource::DataSource;

    let lookup =
        DataSource::with_provider("test", "lookup.Job", "lookup", None).with_binding("lookup");
    let identity_collision =
        Resource::with_provider("test", "managed.Job", "lookup", None).with_binding("managed");
    let mut consumer = Resource::with_provider("test", "consumer.Thing", "consumer", None)
        .with_binding("consumer");
    consumer
        .dependency_bindings
        .insert("lookup_found".to_string());
    let wait = WaitBinding {
        binding: "lookup_found".into(),
        target: "lookup".into(),
        until_raw: "lookup.status == test.lookup.Job.Status.found".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["lookup".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::enum_identifier(
                "test.lookup.Job.Status.found",
            )),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &[identity_collision, consumer],
        &[lookup],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert_eq!(plan.errors().len(), 1);
    assert!(
        matches!(
            &plan.errors()[0].kind,
            PlanErrorKind::WaitTargetIsDataSource {
                wait_binding,
                target,
            } if wait_binding == "lookup_found" && target == "lookup"
        ),
        "expected a data-source target error, got: {:?}",
        plan.errors()
    );
    assert!(!plan.effects().iter().any(Effect::is_wait));
}

#[test]
fn create_plan_records_error_for_empty_predicate_attr_path() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // The wait must gate a pending downstream change, otherwise
    // create_plan legitimately skips emitting or lowering the wait.
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, consumer];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".into())),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        plan.errors().iter().any(|err| {
            matches!(
                &err.kind,
                PlanErrorKind::WaitPredicateInvalid {
                    wait_binding,
                    reason,
                } if wait_binding == "cert_issued" && reason.contains("empty")
            )
        }),
        "expected invalid empty predicate attr path error, got {:?}",
        plan.errors()
    );
}

#[test]
fn wait_provider_satisfier_hint_augments_explicit_dependencies() {
    use crate::effect::Effect;
    use crate::wait::BindingPattern;

    let parsed = crate::parser::parse_and_resolve(
        r#"
        provider aws {
            region = "us-east-1"
        }

        let cert = aws.acm.Certificate {
            domain_name       = "example.com"
            validation_method = "DNS"
        }

        let validation_records = aws.route53.RecordSet {
            name = "_acme.example.com"
        }

        let cert_issued = wait cert {
            until = cert.status == "ISSUED"
        }

        let dist = aws.cloudfront.Distribution {
            cert_status = cert_issued.status
        }
        "#,
    )
    .expect("parsed file should be valid");
    let provider = HintProvider {
        hints: vec![BindingPattern::Exact("validation_records".to_string())],
    };

    let plan = create_plan(
        &parsed.resources,
        &parsed.data_sources,
        &provider,
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &parsed.wait_bindings,
    );

    let Effect::Wait {
        explicit_dependencies,
        ..
    } = plan
        .effects()
        .iter()
        .find(|effect| effect.is_wait())
        .expect("wait should be emitted for changed downstream consumer")
    else {
        unreachable!();
    };
    assert!(
        explicit_dependencies.contains("validation_records"),
        "provider satisfier hint should add validation_records; got {explicit_dependencies:?}"
    );
}

#[test]
fn wait_user_depends_on_conflict_with_provider_hint_deduplicates() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::wait::BindingPattern;

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let dependency = Resource::new("route53.RecordSet", "record").with_binding("something");
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, dependency, consumer];
    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".into())),
        },
        timeout_secs: None,
        depends_on: vec!["something".into()],
        line: 1,
    };
    let provider = HintProvider {
        hints: vec![BindingPattern::Exact("something".to_string())],
    };

    let plan = create_plan(
        &resources,
        &[],
        &provider,
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    let Effect::Wait {
        explicit_dependencies,
        ..
    } = plan
        .effects()
        .iter()
        .find(|effect| effect.is_wait())
        .expect("wait should be emitted")
    else {
        unreachable!();
    };
    assert_eq!(explicit_dependencies.len(), 1);
    assert!(explicit_dependencies.contains("something"));
}

#[test]
fn wait_uses_schema_default_timeout_when_omitted() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // Downstream consumer with a pending change so the wait gates
    // something (carina#3101).
    let mut consumer = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    consumer
        .dependency_bindings
        .insert("cert_issued".to_string());
    let resources = vec![cert, consumer];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::optional("status", AttributeType::string()).build())
            .with_default_wait_timeout(std::time::Duration::from_secs(99))
            .with_default_wait_interval(std::time::Duration::from_secs(7)))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".into())),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );
    let Effect::Wait {
        timeout, interval, ..
    } = plan
        .effects()
        .iter()
        .find(|e| e.is_wait())
        .expect("expected Wait effect")
    else {
        unreachable!();
    };
    assert_eq!(*timeout, std::time::Duration::from_secs(99));
    assert_eq!(*interval, std::time::Duration::from_secs(7));
}

#[test]
fn wait_with_unknown_target_emits_plan_error() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let resources: Vec<Resource> = vec![];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "nonexistent".into(),
        until_raw: "nonexistent.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["nonexistent".to_string(), "status".to_string()],
            // Target lookup must win over predicate sealing even when the
            // parser has correctly typed an unquoted RHS as an enum
            // identifier.
            rhs: Value::Concrete(ConcreteValue::enum_identifier("ISSUED")),
        },
        timeout_secs: None,
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );
    assert!(
        !plan.errors().is_empty(),
        "missing-target wait should surface a plan error"
    );
    assert!(
        matches!(
            &plan.errors()[0].kind,
            PlanErrorKind::WaitTargetMissing { target, .. } if target == "nonexistent"
        ),
        "error message should mention the missing target, got: {}",
        plan.errors()[0]
    );
}

/// carina#3101: a `wait` gates nothing when every downstream consumer
/// is unchanged — no `Effect::Wait` (no lone `> binding (until …)`
/// header on a 0-change plan, no apply-time poll). Mirrors the real
/// `carina-rs/infra registry/dev/registry` shape: cert (unchanged) +
/// distribution (unchanged) referencing `cert_issued`.
#[test]
fn wait_omitted_when_all_consumers_unchanged() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    // Both resources already exist with identical state → NoChange,
    // so neither produces a mutating effect.
    let mut current_states = HashMap::new();
    current_states.insert(
        ResourceId::with_identity("acm.Certificate", "cert"),
        existing_state(
            ResourceId::with_identity("acm.Certificate", "cert"),
            HashMap::new(),
        ),
    );
    current_states.insert(
        ResourceId::with_identity("cloudfront.Distribution", "dist"),
        existing_state(
            ResourceId::with_identity("cloudfront.Distribution", "dist"),
            HashMap::new(),
        ),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".into())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        !plan.effects().iter().any(|e| e.is_wait()),
        "carina#3101: no Effect::Wait when every consumer is unchanged; \
         effects were {:?}",
        plan.effects()
    );
}

/// carina#3101 counterpart (two-faced invariant, like carina#3085):
/// the wait IS emitted when a downstream consumer has a pending change
/// — the dependency-edge behavior (carina#3085 / carina#3061) must not
/// regress just because the no-op case is now suppressed.
#[test]
fn wait_emitted_when_a_consumer_has_a_pending_change() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    // `dist` is new (absent from managed current state) → Create → mutating.
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".into())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        plan.effects().iter().any(
            |e| matches!(e, Effect::Wait { identity, .. } if identity.as_str() == "cert_issued")
        ),
        "carina#3101: the wait must still be emitted when a consumer \
         has a pending change (carina#3085/#3061 not regressed); \
         effects were {:?}",
        plan.effects()
    );
}

/// carina#3358: an already-satisfied wait whose target is unchanged must
/// be elided even when a consumer has a pending change that merely
/// *references* the wait. The wait has no work to do — on `apply` its
/// `until` predicate is already true, so it would poll-and-return
/// immediately, contributing nothing. Mirrors the registry-dev stack:
/// cert (unchanged, already ISSUED) + a changed distribution that wires
/// in `web_acl_id` while still referencing `cert_issued`.
#[test]
fn wait_omitted_when_already_satisfied_and_target_unchanged() {
    use crate::parser::{UntilPredicateAst, WaitBinding};
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};

    // cert: exists with status == ISSUED and is unchanged (desired
    // matches state) → NoChange, no mutating effect on the target.
    let cert = Resource::with_provider("aws", "acm.Certificate", "cert", None)
        .with_binding("cert")
        .with_attribute(
            "status",
            Value::Concrete(ConcreteValue::String("ISSUED".into())),
        );
    // dist is new (absent from managed current state) → Create → mutating.
    // It references the wait binding, so the wait "gates a pending change".
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let mut cert_state_attrs = HashMap::new();
    cert_state_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    );
    let mut current_states = HashMap::new();
    let cert_id = ResourceId::with_provider_identity("aws", "acm.Certificate", "cert", None);
    current_states.insert(
        cert_id.clone(),
        existing_state(cert_id, cert_state_attrs).with_identifier("arn:aws:acm:::certificate/abc"),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == aws.acm.Certificate.Status.issued".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::enum_identifier(
                "aws.acm.Certificate.Status.issued",
            )),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (ResourceSchema::new("acm.Certificate").attribute(
            AttributeSchema::optional(
                "status",
                AttributeType::enum_(
                    TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Status"),
                    Some(vec!["ISSUED".to_string()]),
                    vec![("ISSUED".to_string(), "issued".to_string())],
                    None,
                    None,
                ),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &schemas,
            &resources,
        ),
        &HashMap::new(),
        &schemas,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        !plan.effects().iter().any(|e| e.is_wait()),
        "carina#3358: an already-satisfied wait whose target is unchanged \
         must be elided even when a consumer has a pending change that \
         merely references it; effects were {:?}",
        plan.effects()
    );
    assert!(
        plan.errors().is_empty(),
        "the typed alias must resolve to canonical ISSUED: {:?}",
        plan.errors()
    );
}

/// carina#3358 counterpart: when the wait's target *is* changing in the
/// same plan, the cached state is stale — its post-apply attributes are
/// unknown, so the wait must still be emitted to re-poll on `apply`.
/// Here the target is new and has no current state entry.
#[test]
fn wait_emitted_when_target_is_changing_even_if_cached_state_satisfies() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};

    // cert is new (absent from managed current state) → Create → mutating
    // target. Even though no cached state exists, the wait must poll
    // post-apply.
    let cert = Resource::new("acm.Certificate", "cert").with_binding("cert");
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".into())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &HashMap::new(),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    assert!(
        plan.effects().iter().any(
            |e| matches!(e, Effect::Wait { identity, .. } if identity.as_str() == "cert_issued")
        ),
        "carina#3358: the wait must still be emitted when its target is \
         changing in this plan (cached state is stale); effects were {:?}",
        plan.effects()
    );
}

/// carina#3358: pins the existing-target + `target_is_mutating`
/// branch of `wait_has_work`. An existing target whose cached state
/// already satisfies the predicate, but which has a pending `Update` in
/// this plan, must still emit the wait: the cached `ISSUED` is stale
/// relative to the about-to-apply change, so the executor must re-poll.
/// Without this test the `target_is_mutating ||` half of the gate is
/// dead under the suite and could be silently dropped.
#[test]
fn wait_emitted_when_known_target_has_pending_update_even_if_cached_state_satisfies() {
    use crate::effect::Effect;
    use crate::parser::{UntilPredicateAst, WaitBinding};

    // cert EXISTS in state and its cached `status` already satisfies
    // the predicate, BUT it has a
    // pending Update (desired `domain_name` differs from state).
    let cert = Resource::new("acm.Certificate", "cert")
        .with_binding("cert")
        .with_attribute(
            "domain_name",
            Value::Concrete(ConcreteValue::String("new.example.com".into())),
        );
    let mut dist = Resource::new("cloudfront.Distribution", "dist").with_binding("dist");
    dist.dependency_bindings.insert("cert_issued".to_string());
    let resources = vec![cert, dist];

    let mut cert_state_attrs = HashMap::new();
    cert_state_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    );
    cert_state_attrs.insert(
        "domain_name".to_string(),
        Value::Concrete(ConcreteValue::String("old.example.com".into())),
    );
    let mut current_states = HashMap::new();
    current_states.insert(
        ResourceId::with_identity("acm.Certificate", "cert"),
        existing_state(
            ResourceId::with_identity("acm.Certificate", "cert"),
            cert_state_attrs,
        )
        .with_identifier("arn:aws:acm:::certificate/abc"),
    );

    let wait = WaitBinding {
        binding: "cert_issued".into(),
        target: "cert".into(),
        until_raw: "cert.status == ISSUED".to_string(),
        until_predicate: UntilPredicateAst {
            lhs_segments: vec!["cert".to_string(), "status".to_string()],
            rhs: Value::Concrete(ConcreteValue::String("ISSUED".into())),
        },
        timeout_secs: Some(60),
        depends_on: vec![],
        line: 1,
    };

    let plan = create_plan(
        &resources,
        &[],
        &HintProvider { hints: Vec::new() },
        &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &Orphans::new(),
        &[wait],
    );

    // Precondition: cert really has a pending Update, so this hits the
    // `target_is_mutating` arm for an existing state row and not the
    // `target_needs_wait` arm.
    assert!(
        plan.effects()
            .iter()
            .any(|e| matches!(e, Effect::Update { to, .. }
            if to.id == ResourceId::with_identity("acm.Certificate", "cert"))),
        "test precondition: cert must have a pending Update; effects were {:?}",
        plan.effects()
    );
    assert!(
        plan.effects().iter().any(
            |e| matches!(e, Effect::Wait { identity, .. } if identity.as_str() == "cert_issued")
        ),
        "carina#3358: the wait must still be emitted when an existing \
         (Known) target has a pending Update, even though its cached \
         state satisfies the predicate; effects were {:?}",
        plan.effects()
    );
}
