use super::*;
use crate::parser::ExpandedProviderConfig as ProviderConfig;
use crate::parser::StateBlockAddress;
use crate::resource::{
    AbsoluteBindingAddress, ApplyTimeUnknown, ExpandedAccessPath as AccessPath,
    ExpandedCompositionCall as CompositionCall, ExpandedCompositionCallPath as CompositionCallPath,
    ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
    ExpandedInterpolationPart as InterpolationPart, ExpandedResource as Resource,
    ExpandedValue as Value, LoopModulePrefixOrigin, LoopProvenance, LoopSlot, ModuleInstanceScope,
    ModuleSource, PendingReRead, ReReadReason, RelativeBindingName, ResourceIdentity, RootScope,
    Subscript, ValueString,
};
use crate::schema::{
    DslTransform, ReferenceForm, ReferenceForms, ReferenceTemplate, SchemaRegistry, TypeIdentity,
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, enum_identity,
};
use crate::schema::{
    PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
};
use indexmap::IndexMap;
use std::path::Path;

type LoopValue = crate::resource::Value<LoopSlot>;
type LoopConcreteValue = crate::resource::ConcreteValue<LoopSlot>;
type LoopDeferredValue = crate::resource::DeferredValue<LoopSlot>;
type LoopResource = crate::resource::Resource<ModuleSource, LoopSlot>;

fn absolute_binding(binding: &str) -> AbsoluteBindingAddress {
    RootScope::new().resolve(RelativeBindingName::new(binding))
}

fn concrete_string(value: impl Into<ValueString>) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}

fn canonical_create_only_value_string_for_test(value: &Value) -> Option<String> {
    canonical_create_only_value_string(
        value,
        None,
        crate::schema::empty_defs_for_schema_walks(),
        &crate::differ::exact_comparison_context_for_tests(),
    )
}

fn anonymous_state_entry<I, K>(
    registry: &SchemaRegistry,
    provider: &str,
    resource_type: &str,
    origin_name: &str,
    projected_name: Option<&str>,
    provider_instance: Option<&str>,
    attributes: I,
) -> AnonymousIdStateInfo
where
    I: IntoIterator<Item = (K, Value)>,
    K: Into<String>,
{
    let attributes = attributes
        .into_iter()
        .map(|(name, value)| (name.into(), value))
        .collect::<HashMap<_, _>>();
    let origin_id = ResolvedResourceId::with_provider_identity(
        provider,
        resource_type,
        ResourceIdentity::new(origin_name),
        provider_instance.map(str::to_string),
    );
    let projected_id = projected_name.map(|name| {
        ResolvedResourceId::with_provider_identity(
            provider,
            resource_type,
            ResourceIdentity::new(name),
            None,
        )
    });
    let state_info = registry
        .get(provider, resource_type, crate::schema::SchemaKind::Resource)
        .map(|schema| {
            AnonymousIdStateInfo::try_from_state_row(
                origin_id.clone(),
                projected_id.clone(),
                schema.schema(),
                &attributes,
                &HashMap::new(),
            )
        })
        .unwrap_or_else(|| {
            assert!(
                attributes.is_empty(),
                "state fixtures with comparable attributes require a resource schema"
            );
            AnonymousIdStateInfo::try_without_comparable_attributes(origin_id, projected_id)
        });
    state_info.expect("state fixture projection keeps its provider and resource type")
}

fn anonymous_state_entry_without_attributes(
    registry: &SchemaRegistry,
    provider: &str,
    resource_type: &str,
    name: &str,
) -> AnonymousIdStateInfo {
    anonymous_state_entry(
        registry,
        provider,
        resource_type,
        name,
        None,
        None,
        std::iter::empty::<(&str, Value)>(),
    )
}

fn live_module_source(name: &str, instance: &str) -> ModuleSource {
    ModuleSource::expanded_module(
        name,
        instance,
        ModuleInstanceScope::for_call(name, Some(instance), instance),
        CompositionCallPath::direct(CompositionCall {
            id: crate::parser::ModuleCallId::test_dummy_in(format!("{instance}.crn")),
            module_name: name.to_string(),
            binding: Some(absolute_binding(instance)),
            instance: instance.to_string(),
            module_source: Some(format!("./{name}")),
            module_directory: None,
            loop_origin: None,
        }),
    )
}

fn parse_loop_fixture(source: &str, file: &str) -> Vec<Resource> {
    let parsed = crate::parser::parse_file(
        source,
        Path::new(file),
        &crate::parser::ProviderContext::default(),
    )
    .expect("valid loop fixture");
    crate::module_resolver::resolve_modules_providerless(
        crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
        Path::new("."),
    )
    .expect("loop fixture crosses the module-resolution seam")
    .into_expanded()
    .expect("loop fixture contains no parser placeholders")
    .resources
}

fn loop_identity_registry(stable: bool) -> SchemaRegistry {
    let attribute =
        PatternAttributeSchema::optional("identity_input", PatternAttributeType::string());
    let schema = if stable {
        PatternResourceSchema::new("ec2.Vpc").attribute(attribute.create_only().build())
    } else {
        PatternResourceSchema::new("ec2.Vpc").attribute(attribute.build())
    };
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    registry
}

fn loop_identity_only_registry() -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        PatternResourceSchema::new("ec2.Vpc")
            .attribute(
                PatternAttributeSchema::optional("identity_input", PatternAttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .validate()
            .expect("identity-only schema fixture must be valid"),
    );
    registry
}

fn loop_state_entry(
    registry: &SchemaRegistry,
    name: &str,
    provider_instance: Option<&str>,
    identity_input: Option<&str>,
) -> AnonymousIdStateInfo {
    anonymous_state_entry(
        registry,
        "mock",
        "ec2.Vpc",
        name,
        None,
        provider_instance,
        identity_input
            .map(|value| [("identity_input", concrete_string(value))])
            .into_iter()
            .flatten(),
    )
}

fn detect_legacy_loop_moves_for_test(
    resources: &[Resource],
    registry: &SchemaRegistry,
    state_entries: &[AnonymousIdStateInfo],
    claims: &StateBlockClaims,
    reserved_moves: &[ResolvedMove],
) -> Vec<ResolvedMove> {
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        registry,
        resources.iter().map(|resource| &resource.id),
    );
    let find_state_by_type = |provider: &str, resource_type: &str| {
        claims.screen_entries(
            provider,
            resource_type,
            state_entries
                .iter()
                .filter(|entry| {
                    entry.projected_id().provider() == provider
                        && entry.projected_id().resource_type() == resource_type
                })
                .cloned(),
            AnonymousIdStateInfo::name,
        )
    };
    super::detect_legacy_loop_address_moves(
        resources,
        registry,
        &comparison_provider,
        &find_state_by_type,
        &|_binding| Vec::new(),
        claims,
        reserved_moves,
    )
    .expect("legacy-loop comparison fixture must resolve")
}

#[test]
fn redacted_secret_identifier_material_bytes_are_legacy_compatible() {
    let digest = format!("argon2-legacy:73616c74:{}", "ab".repeat(32));
    let hash = crate::resource::RedactedSecretHash::from_encoded_digest(&digest)
        .expect("valid explicit legacy digest");
    let value = Value::Deferred(DeferredValue::RedactedSecret { hash });

    assert_eq!(
        deterministic_value_string(&value).as_bytes(),
        format!("RedactedSecret(_secret:{digest})").as_bytes(),
    );
}

#[test]
fn pending_reread_identifier_material_has_stable_typed_bytes() {
    let cases = [
        (
            PendingReRead::incomplete("omitted \"name\""),
            r#"Unknown(PendingReRead(Incomplete("omitted \"name\"")))"#,
        ),
        (
            PendingReRead::read_failed("AccessDenied"),
            r#"Unknown(PendingReRead(ReadFailed("AccessDenied")))"#,
        ),
        (
            PendingReRead::new(ReReadReason::NotFound),
            "Unknown(PendingReRead(NotFound))",
        ),
    ];

    for (pending, expected) in cases {
        let value = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
            pending,
        )));
        assert_eq!(
            deterministic_value_string(&value).as_bytes(),
            expected.as_bytes()
        );
    }
}

#[test]
fn unavailable_identifier_material_matches_function_call_and_ignores_diagnostics() {
    let unavailable = |argument: &str, reason: &str, source_line: usize| {
        Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![Value::Concrete(ConcreteValue::String(argument.into())).into()],
            reason: crate::resource::UnavailableReason::from_builtin_error(reason.to_string()),
            source_line: source_line.into(),
        })
    };
    let function_call = |argument: &str| {
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![Value::Concrete(ConcreteValue::String(argument.into())).into()],
            source_line: 0.into(),
        })
    };

    assert_eq!(
        deterministic_value_string(&unavailable("X", "first wording", 1)),
        deterministic_value_string(&unavailable("X", "revised wording", 99)),
    );
    assert_eq!(
        deterministic_value_string(&unavailable("X", "diagnostic only", 7)),
        deterministic_value_string(&function_call("X")),
    );
    assert_ne!(
        deterministic_value_string(&unavailable("X", "same function", 7)),
        deterministic_value_string(&function_call("Y")),
    );
}

fn reconcile_anonymous_identifiers(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> Vec<AnonymousIdStateInfo>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
) -> Vec<ResolvedMove> {
    try_reconcile_anonymous_identifiers(
        resources,
        registry,
        find_state_by_type,
        find_state_by_binding,
    )
    .expect("anonymous reconciliation comparison fixture must resolve")
}

fn try_reconcile_anonymous_identifiers(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> Vec<AnonymousIdStateInfo>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
) -> Result<Vec<ResolvedMove>, AnonymousIdentifierReconciliationError> {
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        registry,
        resources.iter().map(|resource| &resource.id),
    );
    let claims = StateBlockClaims::empty();
    let find_screened_state_by_type = |provider: &str, resource_type: &str| {
        claims.screen_entries(
            provider,
            resource_type,
            find_state_by_type(provider, resource_type),
            AnonymousIdStateInfo::name,
        )
    };
    super::reconcile_anonymous_identifiers(
        resources,
        &[],
        registry,
        &comparison_provider,
        &find_screened_state_by_type,
        find_state_by_binding,
        &claims,
    )
}

fn resolved_move_identity_pairs(moves: &[ResolvedMove]) -> Vec<(String, String)> {
    moves
        .iter()
        .map(|resolved_move| {
            (
                resolved_move.from().identity_str().to_string(),
                resolved_move.to().identity_str().to_string(),
            )
        })
        .collect()
}

fn detect_anonymous_to_named_renames_for_test(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> Vec<AnonymousIdStateInfo>,
    providers: &[ProviderConfig],
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Vec<(ResolvedResourceId, ResolvedResourceId)> {
    detect_anonymous_to_named_renames_with_bindings_for_test(
        resources,
        registry,
        find_state_by_type,
        &|_binding| Vec::new(),
        providers,
        identity_attributes_fn,
    )
}

fn detect_anonymous_to_named_renames_with_bindings_for_test(
    resources: &[Resource],
    registry: &SchemaRegistry,
    find_state_by_type: &dyn Fn(&str, &str) -> Vec<AnonymousIdStateInfo>,
    find_state_by_binding: &dyn Fn(&str) -> Vec<AnonymousIdBindingStateInfo>,
    providers: &[ProviderConfig],
    identity_attributes_fn: &dyn Fn(&str) -> Vec<String>,
) -> Vec<(ResolvedResourceId, ResolvedResourceId)> {
    let claims = StateBlockClaims::empty();
    let find_screened_state_by_type = |provider: &str, resource_type: &str| {
        claims.screen_entries(
            provider,
            resource_type,
            find_state_by_type(provider, resource_type),
            AnonymousIdStateInfo::name,
        )
    };
    super::detect_anonymous_to_named_renames_for_test(
        resources,
        registry,
        &find_screened_state_by_type,
        find_state_by_binding,
        providers,
        identity_attributes_fn,
        &claims,
    )
}

fn make_s3_bucket_schema<R: crate::schema::PatternForm>() -> crate::schema::PatternResourceSchema<R>
{
    PatternResourceSchema::new("s3.Bucket").attribute(
        PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string()).build(),
    )
}

fn registry_with_s3_bucket() -> SchemaRegistry {
    let mut r = SchemaRegistry::new();
    r.insert(
        "awscc",
        (make_s3_bucket_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    r
}

fn provider_config(name: &str, attrs: Vec<(&str, Value)>) -> ProviderConfig {
    ProviderConfig {
        name: name.to_string(),
        attributes: attrs
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }
}

fn region_type<R: crate::schema::PatternForm>(
    provider: &str,
) -> crate::schema::PatternAttributeType<R> {
    PatternAttributeType::enum_(
        enum_identity("Region", Some(provider)),
        None,
        Vec::new(),
        None,
        Some(DslTransform::HyphenToUnderscore),
    )
}

fn eip_domain_type<R: crate::schema::PatternForm>(
    provider: &str,
) -> crate::schema::PatternAttributeType<R> {
    PatternAttributeType::enum_(
        enum_identity("Domain", Some(&format!("{provider}.ec2.Eip"))),
        Some(vec!["vpc".to_string(), "standard".to_string()]),
        Vec::new(),
        None,
        None,
    )
}

fn canonical_eip_domain(provider: &str, raw: &str) -> crate::resource::CanonicalEnumValue {
    let attr_type = eip_domain_type(provider);
    crate::resource::EnumValueResolver::new(&attr_type)
        .resolve_raw(&crate::resource::RawEnumIdentifier::parse(raw))
        .unwrap()
}

const LEGACY_NESTED_ENUM_ANONYMOUS_ID: &str = "awscc_ec2_eip_4823b455";

fn route53_record_set_schema<R: crate::schema::PatternForm>()
-> crate::schema::PatternResourceSchema<R> {
    PatternResourceSchema::new("route53.record_set")
        .attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("hosted_zone_id", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("type", PatternAttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .attribute(PatternAttributeSchema::optional("ttl", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional(
                "resource_records",
                PatternAttributeType::list(PatternAttributeType::string()),
            )
            .build(),
        )
}

fn route53_record_set_resource(record_type: &str) -> Resource {
    let mut resource = Resource::pending_with_provider("awscc", "route53.record_set", None);
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs.dev".into())),
    );
    resource.set_attr(
        "hosted_zone_id".to_string(),
        Value::Concrete(ConcreteValue::String("Z123".into())),
    );
    resource.set_attr(
        "type".to_string(),
        Value::Concrete(ConcreteValue::String(record_type.to_string().into())),
    );
    resource
}

fn route53_state_entry(registry: &SchemaRegistry, name: &str) -> AnonymousIdStateInfo {
    anonymous_state_entry(
        registry,
        "awscc",
        "route53.record_set",
        name,
        None,
        None,
        [
            ("name", concrete_string("carina-rs.dev")),
            ("hosted_zone_id", concrete_string("Z123")),
            ("type", concrete_string("A")),
        ],
    )
}

fn simhash_eip_schema<R: crate::schema::PatternForm>() -> crate::schema::PatternResourceSchema<R> {
    PatternResourceSchema::new("ec2.eip")
        .attribute(
            PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_name", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_env", PatternAttributeType::string()).build(),
        )
}

fn simhash_eip_resource(tag_env: &str) -> Resource {
    let mut resource = Resource::pending_with_provider("awscc", "ec2.eip", None);
    resource.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    resource.set_attr(
        "tag_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-eip".into())),
    );
    resource.set_attr(
        "tag_env".to_string(),
        Value::Concrete(ConcreteValue::String(tag_env.to_string().into())),
    );
    resource
}

fn simhash_suffix_for_test(identifier: &str) -> SimHash {
    match extract_hash_from_identifier(identifier).expect("identifier should have hash suffix") {
        AnonymousHashSuffix::SimHash(hash) => hash,
        AnonymousHashSuffix::Standard(_) => panic!("expected SimHash suffix: {identifier}"),
    }
}

fn budget_identity_schema() -> crate::schema::ResourceSchema {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    ResourceSchema::new("budgets.Budget")
        .attribute(
            AttributeSchema::required(
                "budget",
                AttributeType::struct_(
                    "BudgetData",
                    vec![
                        StructField::optional("budget_name", AttributeType::string())
                            .create_only()
                            .identity()
                            .build(),
                        StructField::required("budget_type", AttributeType::string()).build(),
                    ],
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "notifications_with_subscribers",
                AttributeType::list(AttributeType::string()),
            )
            .create_only()
            .build(),
        )
}

fn budget_resource(name: Option<&str>, notification: &str) -> Resource {
    let mut resource = Resource::pending_with_provider("mock", "budgets.Budget", None);
    let mut budget = IndexMap::new();
    if let Some(name) = name {
        budget.insert("budget_name".to_string(), concrete_string(name));
    }
    budget.insert("budget_type".to_string(), concrete_string("COST"));
    resource.set_attr(
        "budget".to_string(),
        Value::Concrete(ConcreteValue::Map(budget)),
    );
    resource.set_attr(
        "notifications_with_subscribers".to_string(),
        Value::Concrete(ConcreteValue::List(vec![concrete_string(notification)])),
    );
    resource
}

fn record_set_identity_schema() -> crate::schema::ResourceSchema {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    ResourceSchema::new("route53.RecordSet")
        .attribute(
            AttributeSchema::required("hosted_zone_id", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .attribute(
            AttributeSchema::required("type", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .attribute(
            AttributeSchema::optional("aliases", AttributeType::list(AttributeType::string()))
                .create_only()
                .build(),
        )
}

#[test]
fn anonymous_identity_basis_is_unavailable_without_a_schema() {
    let resource: Resource = Resource::pending_with_provider("mock", "test.resource", None);

    assert_eq!(
        classify_anonymous_identity_basis(&resource, &SchemaRegistry::new()),
        None,
    );
}

#[test]
fn schema_less_anonymous_resources_do_not_create_attribute_derived_conflicts() {
    let resources: Vec<Resource> = vec![
        Resource::pending_with_provider("mock", "unknown.Widget", None),
        Resource::pending_with_provider("mock", "unknown.Widget", None),
    ];

    assert!(
        check_attribute_derived_anonymous_resource_conflicts(&resources, &SchemaRegistry::new(),)
            .is_empty(),
        "an unknown identity basis must not be guessed as attribute-derived",
    );
}

#[test]
fn anonymous_identity_basis_is_attribute_derived_when_no_stable_attribute_is_set() {
    let schema = PatternResourceSchema::new("test.resource")
        .attribute(PatternAttributeSchema::optional("name", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut resource: Resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("alpha".into())),
    );

    assert_eq!(
        classify_anonymous_identity_basis(&resource, &registry),
        Some(AnonymousIdentityBasis::AttributeDerived),
    );
}

#[test]
fn anonymous_missing_declared_identity_is_a_typed_error() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let mut resources = vec![budget_resource(None, "actual")];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("missing declared identity must be rejected");

    assert!(matches!(
        error,
        AnonymousIdentityError::IncompleteDeclaredIdentity(_)
    ));
    assert_eq!(
        error.to_string(),
        "Anonymous resource of type 'mock.budgets.Budget' cannot derive an address because \
         identity value 'budget.budget_name' is not available as a written value or authored \
         expression at plan time. Write the identity value or give the resource a name with \
         `let`.",
    );
}

#[test]
fn secret_declared_identity_is_incomplete_and_suggests_a_let_binding() {
    let registry = scalar_identity_registry();
    let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource.set_attr(
        "name",
        Value::Deferred(DeferredValue::Secret(Box::new(concrete_string(
            "must-not-reach-the-address-hash",
        )))),
    );
    let mut resources = vec![resource];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("secret plaintext must not participate in a declared-identity address");

    let AnonymousIdentityError::IncompleteDeclaredIdentity(incomplete) = &error else {
        panic!("expected incomplete declared identity, got {error}");
    };
    assert_eq!(
        incomplete
            .missing_paths()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["name"],
    );
    assert!(error.to_string().contains("name with `let`"), "{error}");
    assert!(resources[0].id.identity_str().is_none());
}

#[test]
fn legacy_create_only_secret_remains_a_stable_identity_input() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.legacy_secret")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("legacy create-only fixture schema must be valid"),
    );
    let mut resource = Resource::pending_with_provider("mock", "test.legacy_secret", None);
    resource.set_attr(
        "name",
        Value::Deferred(DeferredValue::Secret(Box::new(concrete_string(
            "legacy-hash-material",
        )))),
    );
    let mut resources = vec![resource];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("the legacy create-only hash continues to accept secret values");

    assert!(
        resources[0]
            .id
            .identity_str()
            .is_some_and(|identity| identity.starts_with("mock_test_legacy_secret_")),
    );
}

#[test]
fn anonymous_deferred_declared_identity_is_a_typed_error() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let mut resource = LoopResource::pending_with_provider("mock", "budgets.Budget", None);
    resource.set_attr(
        "budget",
        LoopValue::Concrete(LoopConcreteValue::Map(IndexMap::from([
            (
                "budget_name".to_string(),
                LoopValue::Deferred(LoopDeferredValue::Placeholder(LoopSlot::Value)),
            ),
            (
                "budget_type".to_string(),
                LoopValue::Concrete(LoopConcreteValue::String("COST".into())),
            ),
        ]))),
    );
    let mut resources = vec![resource];
    let providers = CanonicalizedProviderConfigs::<LoopSlot>::from_configs_for_test(Vec::new());
    let resources = crate::value::canonicalize_resources_with_schemas(&mut resources, &registry)
        .expect("loop-placeholder identity fixture must canonicalize");

    let error = compute_anonymous_identifiers_with_provider_configs(
        resources,
        &providers,
        &registry,
        AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        &|_| Vec::new(),
    )
    .expect_err("deferred declared identity must be rejected");

    assert!(matches!(
        error,
        AnonymousIdentityError::IncompleteDeclaredIdentity(_)
    ));
}

#[test]
fn anonymous_reference_valued_declared_identity_produces_a_stable_address() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .validate()
            .expect("identity schema fixture must be valid"),
    );
    let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource.set_attr(
        "name",
        Value::root_resource_ref("source", "name", Vec::new()),
    );
    let mut resources = vec![resource];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("a reference expression is a complete declared identity value");

    assert!(
        resources[0]
            .id
            .identity_str()
            .is_some_and(|identity| identity.starts_with("mock_test_resource_")),
        "the anonymous address must be known before the reference resolves",
    );
}

fn scalar_identity_registry() -> SchemaRegistry {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .validate()
            .expect("identity schema fixture must be valid"),
    );
    registry
}

fn when_present_identity_registry() -> SchemaRegistry {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.optional_identity")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .identity_when_present()
                    .build(),
            )
            .validate()
            .expect("when-present identity schema fixture must be valid"),
    );
    registry
}

#[test]
fn when_present_identity_absence_has_a_distinct_address_from_every_written_fixture() {
    let registry = when_present_identity_registry();
    let absent = Resource::pending_with_provider("mock", "test.optional_identity", None);
    let mut empty = Resource::pending_with_provider("mock", "test.optional_identity", None);
    empty.set_attr("name", concrete_string(""));
    let mut marker_like = Resource::pending_with_provider("mock", "test.optional_identity", None);
    marker_like.set_attr("name", concrete_string("Absent"));
    let mut resources = vec![absent, empty, marker_like];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("absence and written values are complete, disjoint identity inputs");

    let identities = resources
        .iter()
        .map(|resource| resource.id.identity_str().expect("computed address"))
        .collect::<BTreeSet<_>>();
    assert_eq!(identities.len(), resources.len());
}

#[test]
fn nested_when_present_identity_distinguishes_an_absent_field_from_an_empty_string() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.nested_optional_identity")
            .attribute(
                AttributeSchema::required(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![
                            StructField::optional("name", AttributeType::string())
                                .create_only()
                                .identity_when_present()
                                .build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .expect("nested when-present identity schema fixture must be valid"),
    );
    let mut absent = Resource::pending_with_provider("mock", "test.nested_optional_identity", None);
    absent.set_attr(
        "settings",
        Value::Concrete(ConcreteValue::Map(IndexMap::new())),
    );
    let mut empty = Resource::pending_with_provider("mock", "test.nested_optional_identity", None);
    empty.set_attr(
        "settings",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "name".to_string(),
            concrete_string(""),
        )]))),
    );
    let mut resources = vec![absent, empty];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("nested absence is a complete identity value");

    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn written_unknown_when_present_identity_remains_incomplete() {
    let registry = when_present_identity_registry();
    let mut resource = Resource::pending_with_provider("mock", "test.optional_identity", None);
    resource.set_attr(
        "name",
        Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
            PendingReRead::new(ReReadReason::NotFound),
        ))),
    );
    let mut resources = vec![resource];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("a written unknown is not the same as an absent identity value");

    assert!(matches!(
        error,
        AnonymousIdentityError::IncompleteDeclaredIdentity(_)
    ));
    assert!(resources[0].id.identity_str().is_none());
}

fn anonymous_identity_for_value(registry: &SchemaRegistry, value: Value) -> ResourceId {
    let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource.set_attr("name", value);
    let mut resources = vec![resource];
    compute_anonymous_identifiers_for_test(&mut resources, &[], registry, &|_| Vec::new())
        .expect("the authored expression is a complete identity value");
    resources.remove(0).id
}

#[test]
fn interpolation_over_reference_declared_identity_has_a_stable_address() {
    let registry = scalar_identity_registry();
    let expression = || {
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("/aws/ecs/".into()),
            InterpolationPart::Expr(Value::root_resource_ref(
                "cluster",
                "cluster_name",
                Vec::new(),
            )),
        ]))
    };

    let first = anonymous_identity_for_value(&registry, expression());
    let second = anonymous_identity_for_value(&registry, expression());

    assert_eq!(first, second);
}

#[test]
fn function_call_over_reference_declared_identity_has_a_stable_address() {
    let registry = scalar_identity_registry();
    let expression = || {
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![Value::root_resource_ref("cluster", "cluster_name", Vec::new()).into()],
            source_line: 0.into(),
        })
    };

    let first = anonymous_identity_for_value(&registry, expression());
    let second = anonymous_identity_for_value(&registry, expression());

    assert_eq!(first, second);
}

#[test]
fn written_and_reference_expression_identity_spellings_do_not_collide() {
    let registry = scalar_identity_registry();
    let mut written = Resource::pending_with_provider("mock", "test.resource", None);
    written.set_attr("name", concrete_string("ResourceRef(zone.id)"));
    let mut referenced = Resource::pending_with_provider("mock", "test.resource", None);
    referenced.set_attr("name", Value::root_resource_ref("zone", "id", Vec::new()));
    let mut resources = vec![written, referenced];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("typed identity values must occupy disjoint hash domains");

    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn written_and_prefix_identity_spellings_do_not_collide() {
    let registry = scalar_identity_registry();
    let mut written = Resource::pending_with_provider("mock", "test.resource", None);
    written.set_attr("name", concrete_string("Prefix(\"app-\")"));
    let mut prefixed = Resource::pending_with_provider("mock", "test.resource", None);
    prefixed
        .prefixes
        .insert("name".to_string(), "app-".to_string());
    let mut resources = vec![written, prefixed];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("prefixes must occupy their own typed identity hash domain");

    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn declared_identity_prefix_expression_hashes_by_spelling_with_distinct_kind() {
    let registry = scalar_identity_registry();
    let reference = || Value::root_resource_ref("prefix_source", "value", Vec::new());
    let mut direct = Resource::pending_with_provider("mock", "test.resource", None);
    direct.set_attr("name", reference());
    let mut prefixed = Resource::pending_with_provider("mock", "test.resource", None);
    prefixed.set_attr("name_prefix", reference());
    let mut resources = vec![direct, prefixed];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("an authored prefix expression is a complete identity input");

    assert_ne!(
        resources[0].id, resources[1].id,
        "an identity value expression and prefix expression need distinct hash tags",
    );
}

#[test]
fn named_resource_is_unaffected_by_missing_declared_identity() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let incomplete = budget_resource(None, "actual");
    let mut named = Resource::with_provider(
        "mock",
        "budgets.Budget",
        ResourceIdentity::new("monthly_budget"),
        None,
    );
    named.binding = Some(absolute_binding("monthly_budget"));
    named.attributes = incomplete.attributes;
    let mut resources = vec![named];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("named resources do not require declared identity for addressing");
    assert_eq!(resources[0].id.identity_str(), Some("monthly_budget"));
}

#[test]
fn anonymous_incomplete_composite_identity_is_classified_as_incomplete() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        record_set_identity_schema()
            .validate()
            .expect("record-set schema fixture must be valid"),
    );
    let mut resource = Resource::pending_with_provider("mock", "route53.RecordSet", None);
    resource.set_attr("hosted_zone_id".to_string(), concrete_string("Z1"));
    resource.set_attr("name".to_string(), concrete_string("www.example.com"));
    // `type`, the third component of the natural key, is intentionally absent.

    assert_eq!(
        classify_anonymous_identity_basis(&resource, &registry),
        Some(AnonymousIdentityBasis::IncompleteDeclaredIdentity),
    );
}

#[test]
fn declared_nested_identity_excludes_mutable_create_only_values_from_address() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );

    let mut before = vec![budget_resource(Some("monthly"), "actual")];
    let mut after = vec![budget_resource(Some("monthly"), "forecasted")];
    compute_anonymous_identifiers_for_test(&mut before, &[], &registry, &|_| Vec::new())
        .expect("first budget identity");
    compute_anonymous_identifiers_for_test(&mut after, &[], &registry, &|_| Vec::new())
        .expect("second budget identity");

    assert_eq!(before[0].id, after[0].id);
}

#[test]
fn different_nested_identity_values_produce_different_addresses() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let mut resources = vec![
        budget_resource(Some("monthly"), "actual"),
        budget_resource(Some("annual"), "actual"),
    ];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("different nested identity values must not collide");

    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn generic_generated_address_collision_is_rejected_across_provider_instances() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required("immutable", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("schema fixture must be valid"),
    );
    let resource = |provider_instance: Option<&str>| {
        let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
        resource.id.provider_instance = provider_instance.map(str::to_string);
        resource.set_attr("immutable", concrete_string("same"));
        resource
    };
    let mut resources = vec![resource(None), resource(Some("west"))];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("a generic generated address must remain globally collision-checked");

    let AnonymousIdentityError::Collision(collision) = error else {
        panic!("expected a typed collision, got {error}");
    };
    assert_eq!(
        collision.kind(),
        AnonymousResourceIdentifierCollisionKind::SharedAddress,
    );
}

#[test]
fn equal_declared_identity_values_are_a_typed_error_suggesting_let() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let mut resources = vec![
        budget_resource(Some("monthly"), "actual"),
        budget_resource(Some("monthly"), "forecasted"),
    ];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("equal declared identity must be rejected");

    let AnonymousIdentityError::Collision(collision) = &error else {
        panic!("expected a typed collision, got {error}");
    };
    assert_eq!(
        collision.kind(),
        AnonymousResourceIdentifierCollisionKind::EqualDeclaredIdentity {
            same_identifier: true,
        },
    );
    assert!(
        error.to_string().contains("equal declared identity values"),
        "{error}",
    );
    assert!(
        error
            .to_string()
            .contains("Give one of them a name with `let`"),
        "{error}",
    );
}

#[test]
fn equal_declared_identity_values_are_rejected_across_provider_instances() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let resource = |provider_instance: Option<&str>| {
        let mut resource = budget_resource(Some("monthly"), "actual");
        resource.id.provider_instance = provider_instance.map(str::to_string);
        resource
    };
    let mut resources = vec![resource(None), resource(Some("west"))];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("state-key aliases across provider instances must be rejected");

    let AnonymousIdentityError::Collision(collision) = &error else {
        panic!("expected a typed collision, got {error}");
    };
    assert_eq!(
        collision.kind(),
        AnonymousResourceIdentifierCollisionKind::EqualDeclaredIdentity {
            same_identifier: true,
        },
    );
    assert!(
        error.to_string().contains("equal declared identity values"),
        "{error}",
    );
}

#[test]
fn equal_declared_identity_values_are_rejected_across_module_scopes() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let mut resources = vec![
        budget_resource(Some("monthly"), "actual")
            .with_module_source(live_module_source("budget", "module_one")),
        budget_resource(Some("monthly"), "forecasted")
            .with_module_source(live_module_source("budget", "module_two")),
    ];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("module address prefixes must not hide equal real-world identities");

    let AnonymousIdentityError::Collision(collision) = &error else {
        panic!("expected a typed collision, got {error}");
    };
    assert_eq!(
        collision.kind(),
        AnonymousResourceIdentifierCollisionKind::EqualDeclaredIdentity {
            same_identifier: false,
        },
    );
    assert!(
        error.to_string().contains("equal declared identity values"),
        "{error}",
    );
}

#[test]
fn composite_identity_hashes_every_record_set_key_component() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        record_set_identity_schema()
            .validate()
            .expect("record-set schema"),
    );
    let record = |zone: &str, name: &str, record_type: &str| {
        let mut resource = Resource::pending_with_provider("mock", "route53.RecordSet", None);
        resource.set_attr("hosted_zone_id".to_string(), concrete_string(zone));
        resource.set_attr("name".to_string(), concrete_string(name));
        resource.set_attr("type".to_string(), concrete_string(record_type));
        resource
    };
    let mut resources = vec![
        record("Z1", "www.example.com", "A"),
        record("Z2", "www.example.com", "A"),
        record("Z1", "api.example.com", "A"),
        record("Z1", "www.example.com", "AAAA"),
    ];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("each complete composite key is distinct");

    let identities = resources
        .iter()
        .map(|resource| resource.id.identity_str().unwrap())
        .collect::<HashSet<_>>();
    assert_eq!(identities.len(), resources.len());
}

fn reference_record_set(zone_binding: &str, name: &str, record_type: &str) -> Resource {
    let mut resource = Resource::pending_with_provider("mock", "route53.RecordSet", None);
    resource.set_attr(
        "hosted_zone_id".to_string(),
        Value::root_resource_ref(zone_binding, "id", Vec::new()),
    );
    resource.set_attr("name".to_string(), concrete_string(name));
    resource.set_attr("type".to_string(), concrete_string(record_type));
    resource
}

#[test]
fn equal_reference_expressions_and_written_identity_values_are_duplicates() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        record_set_identity_schema()
            .validate()
            .expect("record-set schema"),
    );
    let mut resources = vec![
        reference_record_set("zone", "www.example.com", "A"),
        reference_record_set("zone", "www.example.com", "A"),
    ];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("the same reference expression and written key must be a duplicate");

    assert!(matches!(error, AnonymousIdentityError::Collision(_)));
    assert!(
        error.to_string().contains("equal declared identity values"),
        "{error}",
    );
}

#[test]
fn different_reference_expressions_are_distinct_identity_values() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        record_set_identity_schema()
            .validate()
            .expect("record-set schema"),
    );
    let mut resources = vec![
        reference_record_set("primary_zone", "www.example.com", "A"),
        reference_record_set("secondary_zone", "www.example.com", "A"),
    ];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("different authored references remain distinct even if they later resolve equally");

    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn longer_reference_chain_is_a_complete_identity_value() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .identity()
                    .build(),
            )
            .validate()
            .expect("identity schema fixture must be valid"),
    );
    let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource.set_attr(
        "name",
        Value::root_resource_ref("source", "details", vec!["generated_name".to_string()]),
    );
    let mut resources = vec![resource];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("a longer resource-reference chain is complete at plan time");

    assert!(resources[0].id.identity_str().is_some());
}

#[test]
fn longer_reference_chain_resolves_from_state_for_legacy_identity_migration() {
    let reference =
        Value::root_resource_ref("source", "details", vec!["generated_name".to_string()]);
    let source = AnonymousIdBindingStateInfo {
        name: "source".to_string(),
        attribute_values: HashMap::from([(
            "details".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "generated_name".to_string(),
                concrete_string("resolved-name"),
            )]))),
        )]),
    };

    let resolved = resolve_deferred_create_only_value(&reference, &|binding| {
        (binding == "source")
            .then(|| source.clone())
            .into_iter()
            .collect()
    });

    assert_eq!(resolved, Some(concrete_string("resolved-name")));
}

#[test]
fn anonymous_identity_basis_is_stable_when_create_only_value_is_deferred() {
    use crate::resource::AccessPath;

    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut resource: Resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource.set_attr(
        "immutable_name".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root("source", "name"),
        }),
    );

    assert_eq!(
        classify_anonymous_identity_basis(&resource, &registry),
        Some(AnonymousIdentityBasis::Stable),
    );
}

#[test]
fn anonymous_identity_basis_is_stable_when_identity_attribute_prefix_is_set() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
            .create_only()
            .identity()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut resource: Resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource
        .prefixes
        .insert("immutable_name".to_string(), "example-".to_string());

    assert_eq!(
        classify_anonymous_identity_basis(&resource, &registry),
        Some(AnonymousIdentityBasis::Stable),
    );
}

#[test]
fn anonymous_identity_basis_recognizes_unresolved_prefix_syntax() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut resource: Resource = Resource::pending_with_provider("mock", "test.resource", None);
    resource.set_attr(
        "immutable_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("example-".into())),
    );

    assert_eq!(
        classify_anonymous_identity_basis(&resource, &registry),
        Some(AnonymousIdentityBasis::Stable),
    );
}

#[test]
fn identity_attribute_prefix_values_produce_distinct_anonymous_addresses() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
            .create_only()
            .identity()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut alpha = Resource::pending_with_provider("mock", "test.resource", None);
    alpha
        .prefixes
        .insert("immutable_name".to_string(), "alpha-".to_string());
    let mut beta = Resource::pending_with_provider("mock", "test.resource", None);
    beta.prefixes
        .insert("immutable_name".to_string(), "beta-".to_string());
    let mut resources = vec![alpha, beta];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("resolved identity prefixes must produce complete addresses");

    assert!(
        resources
            .iter()
            .all(|resource| resource.id.identity().is_some())
    );
    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn unresolved_identity_prefix_values_produce_distinct_anonymous_addresses() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
            .create_only()
            .identity()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut alpha = Resource::pending_with_provider("mock", "test.resource", None);
    alpha.set_attr(
        "immutable_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("alpha-".into())),
    );
    let mut beta = Resource::pending_with_provider("mock", "test.resource", None);
    beta.set_attr(
        "immutable_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("beta-".into())),
    );
    let mut resources = vec![alpha, beta];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("unresolved identity prefixes must produce complete addresses");

    assert!(
        resources
            .iter()
            .all(|resource| resource.id.identity().is_some())
    );
    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn diagnostic_sensitive_strings_preserve_plaintext_anonymous_identity() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );

    let resource = |name: ValueString| {
        let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
        resource.set_attr(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String(name)),
        );
        resource
    };

    let mut literal = vec![resource("one".into())];
    let mut from_env = vec![resource(ValueString::diagnostic_sensitive("one"))];
    compute_anonymous_identifiers_for_test(&mut literal, &[], &registry, &|_| Vec::new())
        .expect("literal identity");
    compute_anonymous_identifiers_for_test(&mut from_env, &[], &registry, &|_| Vec::new())
        .expect("env-derived identity");

    assert_eq!(from_env[0].id, literal[0].id);
    assert_eq!(
        from_env[0].id.identity_str(),
        Some("mock_test_resource_96081a11"),
        "diagnostic provenance must not change the identifier generated on main",
    );
}

#[test]
fn bare_handle_is_rejected_before_anonymous_identity_hashing() {
    let parsed = crate::parser::parse(
        r#"
            let vpc = mock.test.resource { name = "network" }
            mock.test.Consumer { parent = vpc }
        "#,
        &crate::parser::ProviderContext::default(),
    )
    .expect("the parser must preserve the authored use for typed validation");
    let parsed = crate::module_resolver::resolve_modules_providerless(
        crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
        std::path::Path::new("."),
    )
    .expect("the fixture crosses module resolution before readiness validation");
    let error = crate::validation::resolve_bare_binding_uses(
        parsed,
        &SchemaRegistry::new(),
        &crate::parser::LoadedProviderContext::empty_for_test(),
        &crate::module_resolver::ResolvedModuleSignatures::default(),
    )
    .expect_err("bare handles must never reach identity hashing");

    assert!(
        error
            .to_string()
            .contains("'vpc' is a resource, not a value"),
        "unexpected error: {error}",
    );
}

#[test]
fn distinct_diagnostic_sensitive_strings_produce_distinct_anonymous_identities() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );

    let resource = |name| {
        let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
        resource.set_attr(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
                name,
            ))),
        );
        resource
    };
    let mut resources = vec![resource("one"), resource("two")];

    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("distinct env-derived values must not collide");

    assert_ne!(resources[0].id, resources[1].id);
}

#[test]
fn diagnostic_sensitive_subscript_paths_use_plaintext_for_identity() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("source", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );

    let resource = |key: ValueString| {
        let mut resource = Resource::pending_with_provider("mock", "test.resource", None);
        resource.set_attr(
            "source".to_string(),
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::root_with_fields_and_subscripts(
                    "source",
                    "values",
                    vec![],
                    vec![Subscript::Str { key }],
                ),
            }),
        );
        resource
    };

    let mut literal = vec![resource("one".into())];
    let mut from_env = vec![resource(ValueString::diagnostic_sensitive("one"))];
    compute_anonymous_identifiers_for_test(&mut literal, &[], &registry, &|_| Vec::new())
        .expect("literal subscript identity");
    compute_anonymous_identifiers_for_test(&mut from_env, &[], &registry, &|_| Vec::new())
        .expect("env-derived subscript identity");
    assert_eq!(from_env[0].id, literal[0].id);

    let mut distinct = vec![
        resource(ValueString::diagnostic_sensitive("one")),
        resource(ValueString::diagnostic_sensitive("two")),
    ];
    compute_anonymous_identifiers_for_test(&mut distinct, &[], &registry, &|_| Vec::new())
        .expect("distinct env-derived subscript paths must not collide");
    assert_ne!(distinct[0].id, distinct[1].id);
}

#[test]
fn unresolved_and_resolved_identity_prefix_forms_have_the_same_complete_identity() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
            .create_only()
            .identity()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut unresolved = Resource::pending_with_provider("mock", "test.resource", None);
    unresolved.set_attr(
        "immutable_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("same-".into())),
    );
    let mut resolved = Resource::pending_with_provider("mock", "test.resource", None);
    resolved
        .prefixes
        .insert("immutable_name".to_string(), "same-".to_string());

    let mut unresolved_resources = vec![unresolved];
    let mut resolved_resources = vec![resolved];
    compute_anonymous_identifiers_for_test(&mut unresolved_resources, &[], &registry, &|_| {
        Vec::new()
    })
    .expect("unresolved identity prefix must be complete");
    compute_anonymous_identifiers_for_test(&mut resolved_resources, &[], &registry, &|_| {
        Vec::new()
    })
    .expect("resolved identity prefix must be complete");

    assert_eq!(unresolved_resources[0].id, resolved_resources[0].id);
}

#[test]
fn declared_identity_prefix_is_complete_and_hashes_the_same_before_and_after_resolution() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource").attribute(
        AttributeSchema::optional("immutable_name", AttributeType::string())
            .create_only()
            .identity()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let mut unresolved = Resource::pending_with_provider("mock", "test.resource", None);
    unresolved.set_attr("immutable_name_prefix", concrete_string("service-"));
    let mut resolved = Resource::pending_with_provider("mock", "test.resource", None);
    resolved
        .prefixes
        .insert("immutable_name".to_string(), "service-".to_string());
    resolved.set_attr("immutable_name", concrete_string("service-random"));

    assert_eq!(
        classify_anonymous_identity_basis(&unresolved, &registry),
        Some(AnonymousIdentityBasis::Stable),
    );
    assert_eq!(
        classify_anonymous_identity_basis(&resolved, &registry),
        Some(AnonymousIdentityBasis::Stable),
    );

    let mut unresolved = vec![unresolved];
    let mut resolved = vec![resolved];
    compute_anonymous_identifiers_for_test(&mut unresolved, &[], &registry, &|_| Vec::new())
        .expect("unresolved prefix must produce a complete declared identity");
    compute_anonymous_identifiers_for_test(&mut resolved, &[], &registry, &|_| Vec::new())
        .expect("resolved prefix must produce a complete declared identity");

    assert_eq!(unresolved[0].id, resolved[0].id);
}

#[test]
fn declared_identity_reconciliation_classifies_both_prefix_forms_as_complete() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource").attribute(
        AttributeSchema::optional("immutable_name", AttributeType::string())
            .create_only()
            .identity()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let state_entries = vec![anonymous_state_entry(
        &registry,
        "mock",
        "test.resource",
        "mock_test_resource_11111111",
        None,
        None,
        [("immutable_name", concrete_string("service-existing"))],
    )];
    let unresolved = Resource::with_provider(
        "mock",
        "test.resource",
        ResourceIdentity::new("mock_test_resource_22222222"),
        None,
    )
    .with_attribute("immutable_name_prefix", concrete_string("service-"));
    let mut resolved = Resource::with_provider(
        "mock",
        "test.resource",
        ResourceIdentity::new("mock_test_resource_33333333"),
        None,
    );
    resolved
        .prefixes
        .insert("immutable_name".to_string(), "service-".to_string());
    resolved.set_attr("immutable_name", concrete_string("service-random"));

    for resource in [unresolved, resolved] {
        let destination = resource.id.identity_str().unwrap().to_string();
        let moves = reconcile_anonymous_identifiers(
            std::slice::from_ref(&resource),
            &registry,
            &|_provider, _resource_type| state_entries.clone(),
            &|_binding| Vec::new(),
        );
        assert_eq!(
            resolved_move_identity_pairs(&moves),
            vec![("mock_test_resource_11111111".to_string(), destination)],
        );
    }
}

fn canonical_record_type_identity_schema() -> crate::schema::ResourceSchema {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    ResourceSchema::new("route53.RecordSet").attribute(
        AttributeSchema::required(
            "type",
            AttributeType::enum_(
                enum_identity("RecordType", Some("mock.route53.RecordSet")),
                Some(vec!["A".to_string(), "AAAA".to_string()]),
                Vec::new(),
                None,
                None,
            ),
        )
        .create_only()
        .identity()
        .build(),
    )
}

fn canonical_record_type_resource(value: Value) -> Resource {
    Resource::pending_with_provider("mock", "route53.RecordSet", None).with_attribute("type", value)
}

#[test]
fn declared_identity_hash_canonicalizes_enum_identifier_and_string_values() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        canonical_record_type_identity_schema()
            .validate()
            .expect("record type identity schema must be valid"),
    );
    let mut identifier = vec![canonical_record_type_resource(Value::Concrete(
        ConcreteValue::enum_identifier("a"),
    ))];
    let mut string = vec![canonical_record_type_resource(concrete_string("A"))];

    compute_anonymous_identifiers_for_test(&mut identifier, &[], &registry, &|_| Vec::new())
        .expect("enum identifier identity must compute");
    compute_anonymous_identifiers_for_test(&mut string, &[], &registry, &|_| Vec::new())
        .expect("string identity must compute");

    assert_eq!(identifier[0].id, string[0].id);
}

#[test]
fn equal_canonical_declared_identity_values_report_a_duplicate() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        canonical_record_type_identity_schema()
            .validate()
            .expect("record type identity schema must be valid"),
    );
    let mut resources = vec![
        canonical_record_type_resource(Value::Concrete(ConcreteValue::enum_identifier("a"))),
        canonical_record_type_resource(concrete_string("A")),
    ];

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("canonical-equal declared identity values must be rejected");

    assert!(matches!(error, AnonymousIdentityError::Collision(_)));
    assert!(error.to_string().contains("equal declared identity values"));
}

#[test]
fn check_rejects_multiple_attribute_derived_anonymous_resources_in_one_kind() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        (PatternResourceSchema::new("test.resource").attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let resources: Vec<Resource> = vec![
        Resource::pending_with_provider("mock", "test.resource", None),
        Resource::pending_with_provider("mock", "test.resource", None),
    ];

    let conflicts = check_attribute_derived_anonymous_resource_conflicts(&resources, &registry);

    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0].to_string(),
        "Anonymous resource identity is derived from mutable attributes for multiple \
         'mock.test.resource' declarations using provider instance '<default>' in the root scope. \
         Use `let` bindings to give them distinct stable identities."
    );
}

#[test]
fn check_keeps_default_and_named_provider_instances_in_separate_scopes() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        (PatternResourceSchema::new("test.resource").attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let resources: Vec<Resource> = vec![
        Resource::pending_with_provider("mock", "test.resource", None),
        Resource::pending_with_provider("mock", "test.resource", Some("west".to_string())),
    ];

    let conflicts = check_attribute_derived_anonymous_resource_conflicts(&resources, &registry);

    assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
}

#[test]
fn check_allows_stable_named_and_separate_module_instance_resources() {
    let schema = PatternResourceSchema::new("test.resource").attribute(
        PatternAttributeSchema::optional("immutable_name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    registry.insert(
        "mock",
        (PatternResourceSchema::new("other.resource").attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut stable_one = Resource::pending_with_provider("mock", "test.resource", None);
    stable_one.set_attr(
        "immutable_name".to_string(),
        Value::Concrete(ConcreteValue::String("one".into())),
    );
    let mut stable_two = Resource::pending_with_provider("mock", "test.resource", None);
    stable_two.set_attr(
        "immutable_name".to_string(),
        Value::Concrete(ConcreteValue::String("two".into())),
    );

    let mut named = Resource::pending_with_provider("mock", "other.resource", None);
    named.binding = Some(absolute_binding("named"));

    let module_one = Resource::pending_with_provider("mock", "other.resource", None)
        .with_module_source(live_module_source("fixture", "module_one"));
    let module_two = Resource::pending_with_provider("mock", "other.resource", None)
        .with_module_source(live_module_source("fixture", "module_two"));

    let conflicts = check_attribute_derived_anonymous_resource_conflicts(
        &[stable_one, stable_two, named, module_one, module_two],
        &registry,
    );

    assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
}

#[test]
fn test_anonymous_id_stable_across_provider_namespace_change_in_identity() {
    let schema = PatternResourceSchema::new("ec2.Route")
        .attribute(
            PatternAttributeSchema::optional("route_table_id", PatternAttributeType::string())
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional(
                "destination_cidr_block",
                PatternAttributeType::string(),
            )
            .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

    let make_resource = || {
        let mut resource: Resource = Resource::pending_with_provider("awscc", "ec2.Route", None);
        resource.set_attr(
            "route_table_id".to_string(),
            Value::Concrete(ConcreteValue::String("rtb-123".into())),
        );
        resource.set_attr(
            "destination_cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
        );
        resource
    };

    let providers_awscc = vec![provider_config(
        "awscc",
        vec![(
            "region",
            Value::Concrete(ConcreteValue::enum_identifier(
                "awscc.Region.ap_northeast_1".to_string(),
            )),
        )],
    )];
    let providers_aws = vec![provider_config(
        "awscc",
        vec![(
            "region",
            Value::Concrete(ConcreteValue::enum_identifier(
                "aws.Region.ap_northeast_1".to_string(),
            )),
        )],
    )];
    let config_schema = || {
        ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
            "region".to_string(),
            region_type("aws"),
        )]))
        .unwrap()
    };
    let providers_awscc =
        crate::value::canonicalize_provider_configs_with_schemas(&providers_awscc, &|provider| {
            (provider == "awscc").then(config_schema)
        })
        .unwrap();
    let providers_aws =
        crate::value::canonicalize_provider_configs_with_schemas(&providers_aws, &|provider| {
            (provider == "awscc").then(config_schema)
        })
        .unwrap();

    let mut resources_awscc = vec![make_resource()];
    let mut resources_aws = vec![make_resource()];
    let canonical_awscc =
        crate::value::canonicalize_resources_with_schemas(&mut resources_awscc, &schemas).unwrap();
    let canonical_aws =
        crate::value::canonicalize_resources_with_schemas(&mut resources_aws, &schemas).unwrap();
    compute_anonymous_identifiers_with_provider_configs(
        canonical_awscc,
        &providers_awscc,
        &schemas,
        AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        &identity_fn,
    )
    .unwrap();
    compute_anonymous_identifiers_with_provider_configs(
        canonical_aws,
        &providers_aws,
        &schemas,
        AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        &identity_fn,
    )
    .unwrap();

    assert_eq!(
        resources_awscc[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        resources_aws[0]
            .id
            .identity_str()
            .expect("resolved identity")
    );
}

#[test]
fn test_anonymous_id_stable_across_provider_namespace_change_in_attribute() {
    let schemas_for = |enum_provider: &str| {
        let schema = PatternResourceSchema::new("ec2.Eip").attribute(
            PatternAttributeSchema::optional("domain", eip_domain_type(enum_provider))
                .create_only()
                .build(),
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );
        schemas
    };
    let schemas_awscc = schemas_for("awscc");
    let schemas_aws = schemas_for("aws");
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_resource = |domain: &str| {
        let mut resource = Resource::pending_with_provider("awscc", "ec2.Eip", None);
        resource.set_attr(
            "domain".to_string(),
            Value::Concrete(ConcreteValue::enum_identifier(domain.to_string())),
        );
        resource
    };

    let mut resources_awscc = vec![make_resource("awscc.ec2.Eip.Domain.vpc")];
    let mut resources_aws = vec![make_resource("aws.ec2.Eip.Domain.vpc")];
    compute_anonymous_identifiers_for_test(
        &mut resources_awscc,
        &providers,
        &schemas_awscc,
        &identity_fn,
    )
    .unwrap();
    compute_anonymous_identifiers_for_test(
        &mut resources_aws,
        &providers,
        &schemas_aws,
        &identity_fn,
    )
    .unwrap();

    assert_eq!(
        resources_awscc[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        resources_aws[0]
            .id
            .identity_str()
            .expect("resolved identity")
    );
}

#[test]
fn test_anonymous_id_stable_for_nested_canonical_enum_create_only_values() {
    let schemas_for = |enum_provider: &str| {
        let domain = eip_domain_type(enum_provider);
        let schema = PatternResourceSchema::new("ec2.Eip")
            .attribute(
                PatternAttributeSchema::optional(
                    "domains",
                    PatternAttributeType::list(domain.clone()),
                )
                .create_only()
                .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "domain_by_name",
                    PatternAttributeType::map(domain),
                )
                .create_only()
                .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );
        schemas
    };
    let schemas_awscc = schemas_for("awscc");
    let schemas_aws = schemas_for("aws");
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_resource = |raw: &str| {
        let enum_value = Value::Concrete(ConcreteValue::enum_identifier(raw.to_string()));
        let mut resource = Resource::pending_with_provider("awscc", "ec2.Eip", None);
        resource.set_attr(
            "domains".to_string(),
            Value::Concrete(ConcreteValue::List(vec![enum_value.clone()])),
        );
        resource.set_attr(
            "domain_by_name".to_string(),
            Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                "primary".to_string() => enum_value,
            })),
        );
        resource
    };

    let mut resources_awscc = vec![make_resource("awscc.ec2.Eip.Domain.vpc")];
    let mut resources_aws = vec![make_resource("aws.ec2.Eip.Domain.vpc")];
    compute_anonymous_identifiers_for_test(
        &mut resources_awscc,
        &providers,
        &schemas_awscc,
        &identity_fn,
    )
    .unwrap();
    compute_anonymous_identifiers_for_test(
        &mut resources_aws,
        &providers,
        &schemas_aws,
        &identity_fn,
    )
    .unwrap();

    assert_eq!(
        resources_awscc[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        resources_aws[0]
            .id
            .identity_str()
            .expect("resolved identity")
    );
}

#[test]
fn test_reconcile_anonymous_id_after_provider_namespace_change() {
    let schemas_for = |enum_provider: &str| {
        let schema = PatternResourceSchema::new("ec2.Eip")
            .attribute(
                PatternAttributeSchema::optional("domain", eip_domain_type(enum_provider))
                    .create_only()
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "public_ipv4_pool",
                    PatternAttributeType::string(),
                )
                .create_only()
                .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );
        schemas
    };
    let schemas_awscc = schemas_for("awscc");
    let schemas_aws = schemas_for("aws");
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_resource = |domain: &str, pool: &str| {
        let mut resource = Resource::pending_with_provider("awscc", "ec2.Eip", None);
        resource.set_attr(
            "domain".to_string(),
            Value::Concrete(ConcreteValue::enum_identifier(domain.to_string())),
        );
        resource.set_attr(
            "public_ipv4_pool".to_string(),
            Value::Concrete(ConcreteValue::String(pool.to_string().into())),
        );
        resource
    };

    let mut old_resources = vec![make_resource("awscc.ec2.Eip.Domain.vpc", "pool-a")];
    compute_anonymous_identifiers_for_test(
        &mut old_resources,
        &providers,
        &schemas_awscc,
        &identity_fn,
    )
    .unwrap();
    let state_name = old_resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    let mut desired_resources = vec![make_resource("aws.ec2.Eip.Domain.vpc", "pool-b")];
    compute_anonymous_identifiers_for_test(
        &mut desired_resources,
        &providers,
        &schemas_aws,
        &identity_fn,
    )
    .unwrap();
    assert_ne!(
        state_name,
        desired_resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
    );

    let state_entries = vec![anonymous_state_entry(
        &schemas_aws,
        "awscc",
        "ec2.Eip",
        &state_name,
        None,
        None,
        [
            ("domain", concrete_string("vpc")),
            ("public_ipv4_pool", concrete_string("pool-a")),
        ],
    )];
    let fresh_name = desired_resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let moves = reconcile_anonymous_identifiers(
        &desired_resources,
        &schemas_aws,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        desired_resources[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        fresh_name
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(state_name, fresh_name)]
    );
}

#[test]
fn test_reconcile_anonymous_id_after_nested_enum_create_only_hash_migration() {
    let domain = eip_domain_type("aws");
    let schema = PatternResourceSchema::new("ec2.Eip")
        .attribute(
            PatternAttributeSchema::optional("domains", PatternAttributeType::list(domain))
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("public_ipv4_pool", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_new_resource = |pool: &str| {
        let mut resource = Resource::pending_with_provider("awscc", "ec2.Eip", None);
        resource.set_attr(
            "domains".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::enum_identifier("aws.ec2.Eip.Domain.vpc".to_string()),
            )])),
        );
        resource.set_attr(
            "public_ipv4_pool".to_string(),
            Value::Concrete(ConcreteValue::String(pool.to_string().into())),
        );
        resource
    };

    // This address represents state written by the pre-canonical enum hash
    // path. Current production must not recreate it by bypassing the canonical
    // resource witness.
    let state_name = LEGACY_NESTED_ENUM_ANONYMOUS_ID.to_string();

    let mut desired_resources = vec![make_new_resource("pool-b")];
    compute_anonymous_identifiers_for_test(
        &mut desired_resources,
        &providers,
        &schemas,
        &identity_fn,
    )
    .unwrap();
    assert_ne!(
        state_name,
        desired_resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
    );

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.Eip",
        &state_name,
        None,
        None,
        [
            (
                "domains",
                Value::Concrete(ConcreteValue::List(vec![concrete_string("vpc")])),
            ),
            ("public_ipv4_pool", concrete_string("pool-a")),
        ],
    )];
    let fresh_name = desired_resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let moves = reconcile_anonymous_identifiers(
        &desired_resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        desired_resources[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        fresh_name
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(state_name, fresh_name)]
    );
}

#[test]
fn test_reconcile_anonymous_id_after_nested_enum_no_edit_upgrade_full_match() {
    let domain = eip_domain_type("aws");
    let schema = PatternResourceSchema::new("ec2.Eip")
        .attribute(
            PatternAttributeSchema::optional("domains", PatternAttributeType::list(domain))
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("public_ipv4_pool", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_new_resource = || {
        let mut resource = Resource::pending_with_provider("awscc", "ec2.Eip", None);
        resource.set_attr(
            "domains".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::enum_identifier("aws.ec2.Eip.Domain.vpc".to_string()),
            )])),
        );
        resource.set_attr(
            "public_ipv4_pool".to_string(),
            Value::Concrete(ConcreteValue::String("pool-a".into())),
        );
        resource
    };

    // Keep the old address as persisted input: the current hash API accepts
    // only resources that have crossed canonicalization.
    let state_name = LEGACY_NESTED_ENUM_ANONYMOUS_ID.to_string();

    let mut desired_resources = vec![make_new_resource()];
    compute_anonymous_identifiers_for_test(
        &mut desired_resources,
        &providers,
        &schemas,
        &identity_fn,
    )
    .unwrap();
    assert_ne!(
        state_name,
        desired_resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
    );

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.Eip",
        &state_name,
        None,
        None,
        [
            (
                "domains",
                Value::Concrete(ConcreteValue::List(vec![concrete_string("vpc")])),
            ),
            ("public_ipv4_pool", concrete_string("pool-a")),
        ],
    )];
    let fresh_name = desired_resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let moves = reconcile_anonymous_identifiers(
        &desired_resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        desired_resources[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        fresh_name
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(state_name, fresh_name)]
    );
}

#[test]
fn test_reconcile_anonymous_id_nested_enum_full_match_ambiguous_skips() {
    let domain = eip_domain_type("aws");
    let schema = PatternResourceSchema::new("ec2.Eip")
        .attribute(
            PatternAttributeSchema::optional("domains", PatternAttributeType::list(domain))
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("public_ipv4_pool", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut resource = Resource::with_provider("awscc", "ec2.Eip", "awscc_ec2_eip_b3b9f005", None);
    resource.set_attr(
        "domains".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::enum_identifier("aws.ec2.Eip.Domain.vpc".to_string()),
        )])),
    );
    resource.set_attr(
        "public_ipv4_pool".to_string(),
        Value::Concrete(ConcreteValue::String("pool-a".into())),
    );
    let original_id = resource
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let resources = vec![resource];

    let state_entries = vec![
        anonymous_state_entry(
            &schemas,
            "awscc",
            "ec2.Eip",
            LEGACY_NESTED_ENUM_ANONYMOUS_ID,
            None,
            None,
            [
                (
                    "domains",
                    Value::Concrete(ConcreteValue::List(vec![concrete_string("vpc")])),
                ),
                ("public_ipv4_pool", concrete_string("pool-a")),
            ],
        ),
        anonymous_state_entry(
            &schemas,
            "awscc",
            "ec2.Eip",
            "awscc_ec2_eip_deadbeef",
            None,
            None,
            [
                (
                    "domains",
                    Value::Concrete(ConcreteValue::List(vec![concrete_string("vpc")])),
                ),
                ("public_ipv4_pool", concrete_string("pool-a")),
            ],
        ),
    ];
    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_id
    );
}

#[test]
fn test_reconcile_anonymous_id_mixed_string_and_enum_identifier_for_enum_attribute() {
    let schema = PatternResourceSchema::new("ec2.Subnet")
        .attribute(
            PatternAttributeSchema::optional("region", region_type("awscc"))
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let state_name = "awscc_ec2_subnet_oldhash".to_string();
    let mut resource =
        Resource::with_provider("awscc", "ec2.Subnet", "awscc_ec2_subnet_newhash", None);
    resource.set_attr(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("new-subnet".into())),
    );

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.Subnet",
        &state_name,
        None,
        None,
        [
            ("region", concrete_string("ap-northeast-1")),
            ("name", concrete_string("old-subnet")),
        ],
    )];
    let resources = vec![resource];
    let fresh_name = resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        fresh_name
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(state_name, fresh_name)]
    );
}

#[test]
fn test_generate_random_suffix_format() {
    let suffix = generate_random_suffix();
    assert_eq!(suffix.len(), 8);
    assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_resolve_attr_prefixes_extracts_prefix_and_generates_name() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-".into())),
    );

    let schemas = registry_with_s3_bucket();
    let mut resources = vec![resource];
    resolve_attr_prefixes(&mut resources, &schemas).unwrap();

    // bucket_name_prefix should be removed
    assert!(!resources[0].attributes.contains_key("bucket_name_prefix"));

    // bucket_name should be generated with the prefix
    let bucket_name = match resources[0].get_attr("bucket_name").unwrap() {
        Value::Concrete(ConcreteValue::String(s)) => s.clone(),
        _ => panic!("expected String"),
    };
    assert!(bucket_name.expose_plaintext().starts_with("my-app-"));
    assert_eq!(bucket_name.expose_plaintext().len(), "my-app-".len() + 8); // prefix + 8 hex chars

    // prefixes map should have the entry
    assert_eq!(
        resources[0].prefixes.get("bucket_name"),
        Some(&"my-app-".to_string())
    );
}

#[test]
fn test_resolve_attr_prefixes_leaves_non_matching_prefix_alone() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "nonexistent_attr_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("some-value".into())),
    );

    let schemas = registry_with_s3_bucket();
    let mut resources = vec![resource];
    resolve_attr_prefixes(&mut resources, &schemas).unwrap();

    // nonexistent_attr_prefix should remain untouched
    assert!(
        resources[0]
            .attributes
            .contains_key("nonexistent_attr_prefix")
    );
    assert!(resources[0].prefixes.is_empty());
}

#[test]
fn test_resolve_attr_prefixes_errors_when_both_prefix_and_attr_specified() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-".into())),
    );
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-actual-bucket".into())),
    );

    let schemas = registry_with_s3_bucket();
    let mut resources = vec![resource];
    let result = resolve_attr_prefixes(&mut resources, &schemas);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("cannot specify both"));
}

#[test]
fn test_resolve_attr_prefixes_errors_on_empty_prefix() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource.set_attr(
        "bucket_name_prefix".to_string(),
        Value::Concrete(ConcreteValue::String("".into())),
    );

    let schemas = registry_with_s3_bucket();
    let mut resources = vec![resource];
    let result = resolve_attr_prefixes(&mut resources, &schemas);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("cannot be empty"));
}

#[test]
fn test_reconcile_prefixed_names_reuses_state_name_when_prefix_matches() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-temporary".into())),
    );

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &|_provider, _resource_type, _name| {
        Some(PrefixStateInfo {
            prefixes: vec![("bucket_name".to_string(), "my-app-".to_string())]
                .into_iter()
                .collect(),
            attribute_values: vec![(
                "bucket_name".to_string(),
                PrefixStateValue::Known("my-app-existing1".to_string()),
            )]
            .into_iter()
            .collect(),
        })
    });

    // Should reuse the state name, not the temporary one
    assert_eq!(
        resources[0].get_attr("bucket_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "my-app-existing1".into()
        )))
    );
}

#[test]
fn test_reconcile_prefixed_names_generates_new_name_when_prefix_changes() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource
        .prefixes
        .insert("bucket_name".to_string(), "new-prefix-".to_string());
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("new-prefix-abcd1234".into())),
    );

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &|_provider, _resource_type, _name| {
        Some(PrefixStateInfo {
            prefixes: vec![("bucket_name".to_string(), "old-prefix-".to_string())]
                .into_iter()
                .collect(),
            attribute_values: vec![(
                "bucket_name".to_string(),
                PrefixStateValue::Known("old-prefix-existing1".to_string()),
            )]
            .into_iter()
            .collect(),
        })
    });

    // Should keep the newly generated name since prefix changed
    assert_eq!(
        resources[0].get_attr("bucket_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "new-prefix-abcd1234".into()
        )))
    );
}

#[test]
fn test_reconcile_prefixed_names_keeps_generated_name_when_no_state() {
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-app-abcd1234".into())),
    );

    let mut resources = vec![resource];
    reconcile_prefixed_names(&mut resources, &|_provider, _resource_type, _name| None);

    // No state, so keep the generated name
    assert_eq!(
        resources[0].get_attr("bucket_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "my-app-abcd1234".into()
        )))
    );
}

#[test]
fn test_reconcile_anonymous_id_partial_create_only_match() {
    // When one create-only property changes but another stays the same,
    // reconciliation should move the state row to the fresh identifier.
    let schema = PatternResourceSchema::new("iam.role")
        .attribute(
            PatternAttributeSchema::optional("role_name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("path", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    // Step 1: compute identifier with path="/"
    let mut r1 = Resource::pending_with_provider("awscc", "iam.role", None);
    r1.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-role".into())),
    );
    r1.set_attr(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String("/".into())),
    );
    let mut resources1 = vec![r1];
    compute_anonymous_identifiers_for_test(&mut resources1, &providers, &schemas, &identity_fn)
        .unwrap();
    let step1_id = resources1[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Step 2: compute identifier with path="/carina/" (changed create-only)
    let mut r2 = Resource::pending_with_provider("awscc", "iam.role", None);
    r2.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-role".into())),
    );
    r2.set_attr(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String("/carina/".into())),
    );
    let mut resources2 = vec![r2];
    compute_anonymous_identifiers_for_test(&mut resources2, &providers, &schemas, &identity_fn)
        .unwrap();
    let step2_id = resources2[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Hash includes path, so identifiers differ
    assert_ne!(step1_id, step2_id);

    // Reconcile: state has role_name="my-role" (match) and path="/" (mismatch)
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "iam.role",
        &step1_id,
        None,
        None,
        [
            ("role_name", concrete_string("my-role")),
            ("path", concrete_string("/")),
        ],
    )];
    let moves = reconcile_anonymous_identifiers(
        &resources2,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Reconciliation preserves the freshly computed identity and migrates state to it.
    assert_eq!(
        resources2[0].id.identity_str().expect("resolved identity"),
        step2_id
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(step1_id, step2_id)]
    );
}

#[test]
fn state_row_identity_values_use_the_original_permanent_name_override() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.unique_identity")
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .with_unique_name_attribute("name")
        .validate()
        .expect("schema fixture must be valid");
    let mut desired = Resource::pending_with_provider("mock", "test.unique_identity", None);
    desired.set_attr("name", concrete_string("role"));
    let desired_values = DesiredIdentityValues::from_resource(&desired, &schema, &|_| Vec::new());
    let attributes = HashMap::from([("name".to_string(), concrete_string("role-a1b2c3d4"))]);
    let without_override =
        StateRowIdentityValues::from_state_row(&schema, &attributes, &HashMap::new());
    let with_override = StateRowIdentityValues::from_state_row(
        &schema,
        &attributes,
        &HashMap::from([(
            "name".to_string(),
            crate::name_override::NameOverride {
                temp_value: "role-a1b2c3d4".to_string(),
                original_value: Some("role".to_string()),
            },
        )]),
    );
    let context = crate::differ::exact_comparison_context_for_tests();
    assert_eq!(
        desired_values.compare_row(&without_override, &schema, &context),
        DeclaredIdentityRowOutcome::Mismatch,
    );
    assert_eq!(
        desired_values.compare_row(&with_override, &schema, &context),
        DeclaredIdentityRowOutcome::Match,
    );
}

#[test]
fn reconcile_unknown_name_override_origin_requires_an_explicit_moved_block() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.unique_identity")
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .with_unique_name_attribute("name")
        .validate()
        .expect("schema fixture must be valid");
    let mut registry = SchemaRegistry::new();
    registry.insert("mock", schema);

    let mut desired = Resource::with_provider(
        "mock",
        "test.unique_identity",
        "mock_test_unique_identity_22222222",
        None,
    );
    desired.set_attr("name", concrete_string("role"));

    let origin_id = ResolvedResourceId::with_provider_identity(
        "mock",
        "test.unique_identity",
        "mock_test_unique_identity_11111111",
        None,
    );
    let state_entry = AnonymousIdStateInfo::try_from_state_row(
        origin_id,
        None,
        registry
            .get(
                "mock",
                "test.unique_identity",
                crate::schema::SchemaKind::Resource,
            )
            .expect("registered schema")
            .schema(),
        &HashMap::from([("name".to_string(), concrete_string("role-a1b2c3d4"))]),
        &HashMap::from([(
            "name".to_string(),
            crate::name_override::NameOverride {
                temp_value: "role-a1b2c3d4".to_string(),
                original_value: None,
            },
        )]),
    )
    .expect("state fixture is comparable by provider and type");

    let error = try_reconcile_anonymous_identifiers(
        &[desired],
        &registry,
        &|_provider, _resource_type| vec![state_entry.clone()],
        &|_binding| Vec::new(),
    )
    .expect_err("an unknown override origin must not silently become delete plus create");

    let AnonymousIdentifierReconciliationError::UnknownIdentityOverrideOrigin(error) = error else {
        panic!("expected typed unknown override-origin error, got {error}");
    };
    assert_eq!(error.candidate_rows().len(), 1);
    assert_eq!(error.candidate_rows()[0].attributes(), ["name"]);
    let message = error.to_string();
    assert!(
        message.contains("mock_test_unique_identity_11111111"),
        "{message}",
    );
    assert!(message.contains("moved"), "{message}");
}

#[test]
fn reconcile_override_hidden_identity_row_with_comparable_mismatch_is_ignored() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.unique_identity")
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .attribute(
            AttributeSchema::required("scope", AttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .with_unique_name_attribute("name")
        .validate()
        .expect("schema fixture must be valid");
    let mut registry = SchemaRegistry::new();
    registry.insert("mock", schema);

    let mut desired = Resource::with_provider(
        "mock",
        "test.unique_identity",
        "mock_test_unique_identity_22222222",
        None,
    );
    desired.set_attr("name", concrete_string("new-role"));
    desired.set_attr("scope", concrete_string("new-scope"));

    let origin_id = ResolvedResourceId::with_provider_identity(
        "mock",
        "test.unique_identity",
        "mock_test_unique_identity_11111111",
        None,
    );
    let state_entry = AnonymousIdStateInfo::try_from_state_row(
        origin_id,
        None,
        registry
            .get(
                "mock",
                "test.unique_identity",
                crate::schema::SchemaKind::Resource,
            )
            .expect("registered schema")
            .schema(),
        &HashMap::from([
            ("name".to_string(), concrete_string("old-role-temporary")),
            ("scope".to_string(), concrete_string("old-scope")),
        ]),
        &HashMap::from([(
            "name".to_string(),
            crate::name_override::NameOverride {
                temp_value: "old-role-temporary".to_string(),
                original_value: None,
            },
        )]),
    )
    .expect("state fixture is comparable by provider and type");

    let moves = try_reconcile_anonymous_identifiers(
        &[desired],
        &registry,
        &|_provider, _resource_type| vec![state_entry.clone()],
        &|_binding| Vec::new(),
    )
    .expect("a comparable identity mismatch makes the orphan an ordinary delete");

    assert!(moves.is_empty());
}

#[test]
fn full_and_partial_create_only_matches_emit_moves_without_changing_resource_ids() {
    let schema = PatternResourceSchema::new("iam.role")
        .attribute(
            PatternAttributeSchema::optional("role_name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("path", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let resource = |identity: &str, role_name: &str, path: &str, instance: Option<&str>| {
        let mut resource = Resource::with_provider(
            "awscc",
            "iam.role",
            ResourceIdentity::new(identity.to_string()),
            instance.map(str::to_string),
        );
        resource.set_attr(
            "role_name".to_string(),
            Value::Concrete(ConcreteValue::String(role_name.into())),
        );
        resource.set_attr(
            "path".to_string(),
            Value::Concrete(ConcreteValue::String(path.into())),
        );
        resource
    };

    let resources = vec![
        resource("iam_role_aabbccdd", "full-role", "/full/", None),
        resource(
            "iam_role_eeff0011",
            "partial-role",
            "/partial-new/",
            Some("west"),
        ),
    ];
    let original_ids: Vec<ResourceId> = resources
        .iter()
        .map(|resource| resource.id.clone())
        .collect();
    let state_entries = vec![
        anonymous_state_entry(
            &schemas,
            "awscc",
            "iam.role",
            "iam_role_11223344",
            None,
            None,
            [
                ("role_name", concrete_string("full-role")),
                ("path", concrete_string("/full/")),
            ],
        ),
        anonymous_state_entry(
            &schemas,
            "awscc",
            "iam.role",
            "iam_role_55667788",
            None,
            Some("east"),
            [
                ("role_name", concrete_string("partial-role")),
                ("path", concrete_string("/partial-old/")),
            ],
        ),
    ];

    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources
            .iter()
            .map(|resource| resource.id.clone())
            .collect::<Vec<_>>(),
        original_ids
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![
            (
                "iam_role_11223344".to_string(),
                "iam_role_aabbccdd".to_string(),
            ),
            (
                "iam_role_55667788".to_string(),
                "iam_role_eeff0011".to_string(),
            ),
        ]
    );
    assert_eq!(moves[1].from().provider_instance(), Some("east"));
    assert_eq!(moves[1].to().provider_instance(), Some("west"));
}

#[test]
fn test_reconcile_anonymous_id_no_match_when_all_differ() {
    // When ALL create-only properties differ, no reconciliation (truly new resource)
    let schema = PatternResourceSchema::new("iam.role")
        .attribute(
            PatternAttributeSchema::optional("role_name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("path", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut resource = Resource::with_provider("awscc", "iam.role", "iam_role_aabbccdd", None);
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("new-role".into())),
    );
    resource.set_attr(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String("/new/".into())),
    );

    let original_id = resource
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let resources = vec![resource];

    // State has completely different values
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "iam.role",
        "iam_role_11223344",
        None,
        None,
        [
            ("role_name", concrete_string("old-role")),
            ("path", concrete_string("/old/")),
        ],
    )];
    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Identifier should remain unchanged
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_id
    );
}

#[test]
fn test_reconcile_anonymous_id_unique_full_match_emits_move() {
    // A unique full match covers no-edit upgrades where the anonymous hash input
    // changed across Carina versions but all create-only values are unchanged.
    let schema = PatternResourceSchema::new("iam.role")
        .attribute(
            PatternAttributeSchema::optional("role_name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("path", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut resource = Resource::with_provider("awscc", "iam.role", "iam_role_aabbccdd", None);
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-role".into())),
    );
    resource.set_attr(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String("/".into())),
    );

    let resources = vec![resource];

    // State has the same values but a different ID from a previous hash scheme.
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "iam.role",
        "iam_role_11223344",
        None,
        None,
        [
            ("role_name", concrete_string("my-role")),
            ("path", concrete_string("/")),
        ],
    )];
    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        "iam_role_aabbccdd"
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(
            "iam_role_11223344".to_string(),
            "iam_role_aabbccdd".to_string()
        )]
    );
}

#[test]
fn test_reconcile_anonymous_id_single_create_only_no_reconcile() {
    // With only one create-only property, changing it means ALL changed,
    // so no reconciliation (matched=0 or mismatched=0)
    let schema = PatternResourceSchema::new("ec2.Vpc").attribute(
        PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "ec2_vpc_aabbccdd", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
    );

    let original_id = resource
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let resources = vec![resource];

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.Vpc",
        "ec2_vpc_11223344",
        None,
        None,
        [("cidr_block", concrete_string("10.0.0.0/16"))],
    )];
    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // No reconciliation: only one create-only prop and it changed
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_id
    );
}

/// Anonymous resources declared inside a module instantiation must
/// surface the owning instance in their final identifier so the plan
/// address shows the module they belong to (#2516). Without the prefix
/// `iam_role_policy_<hash>` is indistinguishable from a top-level
/// anonymous resource.
#[test]
fn test_anonymous_resource_inside_module_keeps_instance_prefix() {
    let schema = PatternResourceSchema::new("iam.RolePolicy").attribute(
        PatternAttributeSchema::optional("policy_name", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: vec![(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        )]
        .into_iter()
        .collect(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

    let mut r = Resource::pending_with_provider("awscc", "iam.RolePolicy", None);
    r.set_attr(
        "policy_name".to_string(),
        Value::Concrete(ConcreteValue::String("inline".into())),
    );
    r.module_source = Some(live_module_source("github_oidc", "bootstrap"));

    let mut resources = vec![r];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();

    let name = resources[0].id.identity_str().expect("resolved identity");
    assert!(
        name.starts_with("bootstrap."),
        "expected `bootstrap.<hash>` prefix, got {:?}",
        name,
    );
    assert!(
        name.contains("awscc_iam_role_policy_"),
        "expected hash-based identifier suffix, got {:?}",
        name,
    );
}

#[test]
fn test_anonymous_resource_no_create_only_properties() {
    // Resources with no create-only properties should still work as anonymous resources
    let schema = PatternResourceSchema::new("ec2.eip")
        .attribute(
            PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional(
                "tags",
                PatternAttributeType::map(PatternAttributeType::string()),
            )
            .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: vec![(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        )]
        .into_iter()
        .collect(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

    let mut r = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );

    let mut resources = vec![r];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();

    // Should have computed an identifier
    assert!(
        !resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    assert!(
        resources[0]
            .id
            .identity_str()
            .unwrap_or("")
            .starts_with("awscc_ec2_eip_")
    );
}

#[test]
fn test_anonymous_resource_no_create_only_deterministic() {
    // Same attributes should produce the same identifier
    let schema = PatternResourceSchema::new("ec2.eip").attribute(
        PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: vec![(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        )]
        .into_iter()
        .collect(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

    let make_resource = || {
        let mut r = Resource::pending_with_provider("awscc", "ec2.eip", None);
        r.set_attr(
            "domain".to_string(),
            Value::Concrete(ConcreteValue::String("vpc".into())),
        );
        r
    };

    let mut resources1 = vec![make_resource()];
    let mut resources2 = vec![make_resource()];
    compute_anonymous_identifiers_for_test(&mut resources1, &providers, &schemas, &identity_fn)
        .unwrap();
    compute_anonymous_identifiers_for_test(&mut resources2, &providers, &schemas, &identity_fn)
        .unwrap();

    assert_eq!(
        resources1[0].id.identity_str().expect("resolved identity"),
        resources2[0].id.identity_str().expect("resolved identity")
    );
}

#[test]
fn test_anonymous_resource_no_create_only_collision() {
    // Two identical anonymous resources with no create-only properties should collide
    let schema = PatternResourceSchema::new("ec2.eip").attribute(
        PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut r1 = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r1.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );

    let mut r2 = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r2.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );

    let mut resources = vec![r1, r2];
    let result =
        compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn);

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("collision"));
}

#[test]
fn test_identity_attribute_prevents_collision() {
    // Two anonymous resources of the same type with the same create-only attrs
    // but different identity attrs should NOT collide.
    // This simulates route53.record_set where `name` is create-only (same)
    // but `type` is identity (A vs AAAA).
    let schema = PatternResourceSchema::new("route53.record_set")
        .attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("hosted_zone_id", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("type", PatternAttributeType::string())
                .create_only()
                .identity()
                .build(),
        )
        .attribute(PatternAttributeSchema::optional("ttl", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional(
                "resource_records",
                PatternAttributeType::list(PatternAttributeType::string()),
            )
            .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut r1 = Resource::pending_with_provider("awscc", "route53.record_set", None);
    r1.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs.dev".into())),
    );
    r1.set_attr(
        "hosted_zone_id".to_string(),
        Value::Concrete(ConcreteValue::String("Z123".into())),
    );
    r1.set_attr(
        "type".to_string(),
        Value::Concrete(ConcreteValue::String("A".into())),
    );

    let mut r2 = Resource::pending_with_provider("awscc", "route53.record_set", None);
    r2.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs.dev".into())),
    );
    r2.set_attr(
        "hosted_zone_id".to_string(),
        Value::Concrete(ConcreteValue::String("Z123".into())),
    );
    r2.set_attr(
        "type".to_string(),
        Value::Concrete(ConcreteValue::String("AAAA".into())),
    );

    let mut resources = vec![r1, r2];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .expect("should not collide when identity attrs differ");

    // Both should have identifiers assigned
    assert!(
        !resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    assert!(
        !resources[1]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    // Identifiers should be different
    assert_ne!(
        resources[0].id.identity_str().expect("resolved identity"),
        resources[1].id.identity_str().expect("resolved identity"),
        "different identity attr values should produce different identifiers"
    );
}

#[test]
fn reconcile_identity_declared_address_migration_matches_nested_state_identity() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let mut desired = vec![budget_resource(Some("monthly"), "forecasted")];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("desired budget identity");
    let destination = desired[0].id.identity_str().unwrap().to_string();
    let old_address = "mock_budgets_budget_0ddc0ffe";
    assert_ne!(old_address, destination);

    let mut budget = IndexMap::new();
    budget.insert("budget_name".to_string(), concrete_string("monthly"));
    budget.insert("budget_type".to_string(), concrete_string("COST"));
    let state_entries = vec![anonymous_state_entry(
        &registry,
        "mock",
        "budgets.Budget",
        old_address,
        None,
        None,
        [
            ("budget", Value::Concrete(ConcreteValue::Map(budget))),
            (
                "notifications_with_subscribers",
                Value::Concrete(ConcreteValue::List(vec![concrete_string("actual")])),
            ),
        ],
    )];

    let moves = reconcile_anonymous_identifiers(
        &desired,
        &registry,
        &|_provider, _resource_type| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(old_address.to_string(), destination)],
    );
}

#[test]
fn reconcile_when_present_absence_matches_only_a_state_row_with_the_attribute_absent() {
    let registry = when_present_identity_registry();
    let mut desired = vec![Resource::pending_with_provider(
        "mock",
        "test.optional_identity",
        None,
    )];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("absent when-present identity must produce a destination address");
    let destination = desired[0].id.identity_str().unwrap().to_string();
    let absent_row = "mock_test_optional_identity_11111111";
    let written_row = "mock_test_optional_identity_22222222";
    let state_entries = vec![
        anonymous_state_entry(
            &registry,
            "mock",
            "test.optional_identity",
            absent_row,
            None,
            None,
            std::iter::empty::<(&str, Value)>(),
        ),
        anonymous_state_entry(
            &registry,
            "mock",
            "test.optional_identity",
            written_row,
            None,
            None,
            [("name", concrete_string(""))],
        ),
    ];

    let moves = reconcile_anonymous_identifiers(
        &desired,
        &registry,
        &|_provider, _resource_type| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(absent_row.to_string(), destination)],
    );
}

fn redacted_identity_value() -> Value {
    let digest = format!("argon2-legacy:73616c74:{}", "ab".repeat(32));
    let hash = crate::resource::RedactedSecretHash::from_encoded_digest(&digest)
        .expect("valid persisted secret digest");
    Value::Deferred(DeferredValue::RedactedSecret { hash })
}

#[test]
fn reconcile_when_present_absence_does_not_match_a_redacted_secret() {
    let registry = when_present_identity_registry();
    let mut desired = vec![Resource::pending_with_provider(
        "mock",
        "test.optional_identity",
        None,
    )];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("absent when-present identity must produce a destination address");
    let state_entry = anonymous_state_entry(
        &registry,
        "mock",
        "test.optional_identity",
        "mock_test_optional_identity_11111111",
        None,
        None,
        [("name", redacted_identity_value())],
    );
    let schema = registry.get_for(&desired[0]).unwrap();
    let schema = schema.schema();
    let desired_values = DesiredIdentityValues::from_resource(&desired[0], schema, &|_| Vec::new());

    assert_eq!(
        desired_values.compare_row(
            &state_entry.identity_values,
            schema,
            &crate::differ::exact_comparison_context_for_tests(),
        ),
        DeclaredIdentityRowOutcome::Mismatch,
        "a persisted redacted secret is present even though it cannot be compared",
    );
    let moves = reconcile_anonymous_identifiers(
        &desired,
        &registry,
        &|_provider, _resource_type| vec![state_entry.clone()],
        &|_binding| Vec::new(),
    );
    assert!(
        moves.is_empty(),
        "a present secret must not migrate to absence"
    );
}

#[test]
fn reconcile_when_present_absence_selects_the_absent_row_beside_a_redacted_secret() {
    let registry = when_present_identity_registry();
    let mut desired = vec![Resource::pending_with_provider(
        "mock",
        "test.optional_identity",
        None,
    )];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("absent when-present identity must produce a destination address");
    let destination = desired[0].id.identity_str().unwrap().to_string();
    let absent_row = "mock_test_optional_identity_22222222";
    let state_entries = vec![
        anonymous_state_entry(
            &registry,
            "mock",
            "test.optional_identity",
            "mock_test_optional_identity_11111111",
            None,
            None,
            [("name", redacted_identity_value())],
        ),
        anonymous_state_entry_without_attributes(
            &registry,
            "mock",
            "test.optional_identity",
            absent_row,
        ),
    ];

    let moves = try_reconcile_anonymous_identifiers(
        &desired,
        &registry,
        &|_provider, _resource_type| state_entries.clone(),
        &|_binding| Vec::new(),
    )
    .expect("the secret row must not make the absent row ambiguous");

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(absent_row.to_string(), destination)],
    );
}

#[test]
fn nested_when_present_absence_does_not_match_redacted_state() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let schema = ResourceSchema::new("test.nested_optional_identity")
        .attribute(
            AttributeSchema::required(
                "settings",
                AttributeType::struct_(
                    "Settings",
                    vec![
                        StructField::optional("name", AttributeType::string())
                            .create_only()
                            .identity_when_present()
                            .build(),
                    ],
                ),
            )
            .build(),
        )
        .validate()
        .expect("nested when-present identity schema must be valid");
    let desired = Resource::pending_with_provider("mock", "test.nested_optional_identity", None)
        .with_attribute(
            "settings",
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        );
    let desired_values = DesiredIdentityValues::from_resource(&desired, &schema, &|_| Vec::new());
    let redacted_leaf = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "name".to_string(),
        redacted_identity_value(),
    )])));

    for settings in [redacted_leaf, redacted_identity_value()] {
        let state = StateRowIdentityValues::from_state_row(
            &schema,
            &HashMap::from([("settings".to_string(), settings)]),
            &HashMap::new(),
        );
        assert_eq!(
            desired_values.compare_row(
                &state,
                &schema,
                &crate::differ::exact_comparison_context_for_tests(),
            ),
            DeclaredIdentityRowOutcome::Mismatch,
            "a redacted leaf or ancestor cannot prove that the path is absent",
        );
    }
}

#[test]
fn declared_identity_resolved_and_prefix_values_are_undecidable_against_redacted_state() {
    let registry = when_present_identity_registry();
    let resolved = Resource::pending_with_provider("mock", "test.optional_identity", None)
        .with_attribute("name", concrete_string("service-existing"));
    let prefix = Resource::pending_with_provider("mock", "test.optional_identity", None)
        .with_attribute("name_prefix", concrete_string("service-"));

    for desired in [resolved, prefix] {
        let schema = registry.get_for(&desired).unwrap();
        let schema = schema.schema();
        let desired_values =
            DesiredIdentityValues::from_resource(&desired, schema, &|_| Vec::new());
        let state = StateRowIdentityValues::from_state_row(
            schema,
            &HashMap::from([("name".to_string(), redacted_identity_value())]),
            &HashMap::new(),
        );
        let context = crate::differ::exact_comparison_context_for_tests();

        assert!(matches!(
            desired_values.compare_row(&state, schema, &context),
            DeclaredIdentityRowOutcome::Undecidable(_),
        ));
        let absent =
            StateRowIdentityValues::from_state_row(schema, &HashMap::new(), &HashMap::new());
        assert_eq!(
            desired_values.compare_row(&absent, schema, &context),
            DeclaredIdentityRowOutcome::Mismatch,
        );
    }
}

#[test]
fn reconcile_declared_identity_migration_evaluates_interpolation_over_state_backed_reference() {
    let registry = scalar_identity_registry();
    let mut desired = Resource::pending_with_provider("mock", "test.resource", None);
    desired.set_attr(
        "name",
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::root_resource_ref("zone", "name", Vec::new())),
            InterpolationPart::Literal("-x".into()),
        ])),
    );
    let mut desired = vec![desired];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("authored interpolation must have an address before migration");
    let destination = desired[0]
        .id
        .identity_str()
        .expect("computed destination")
        .to_string();
    let legacy = "mock_test_resource_11111111";
    let state_entries = vec![anonymous_state_entry(
        &registry,
        "mock",
        "test.resource",
        legacy,
        None,
        None,
        [("name", concrete_string("example-x"))],
    )];
    let binding_entries = [binding_state_entry("zone", "zone", &[("name", "example")])];

    let moves = reconcile_anonymous_identifiers(
        &desired,
        &registry,
        &|_provider, _resource_type| state_entries.clone(),
        &|binding| binding_state_entries_for(&binding_entries, binding),
    );

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(legacy.to_string(), destination)],
    );
}

#[test]
fn reconcile_unresolved_declared_identity_with_legacy_candidate_requires_moved() {
    let registry = scalar_identity_registry();
    let mut desired = Resource::pending_with_provider("mock", "test.resource", None);
    desired.set_attr(
        "name",
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::root_resource_ref("missing_zone", "name", Vec::new())),
            InterpolationPart::Literal("-x".into()),
        ])),
    );
    let mut desired = vec![desired];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("authored interpolation must have an address before migration");
    let legacy = "mock_test_resource_11111111";
    let state_entries = vec![anonymous_state_entry(
        &registry,
        "mock",
        "test.resource",
        legacy,
        None,
        None,
        [("name", concrete_string("example-x"))],
    )];

    let error = try_reconcile_anonymous_identifiers(
        &desired,
        &registry,
        &|_provider, _resource_type| state_entries.clone(),
        &|_binding| Vec::new(),
    )
    .expect_err("an unresolved identity with an unclaimed legacy row needs an explicit move");

    let AnonymousIdentifierReconciliationError::UnresolvedDeclaredIdentity(error) = error else {
        panic!("expected typed unresolved declared-identity error, got {error}");
    };
    assert_eq!(error.candidate_rows().len(), 1);
    let message = error.to_string();
    assert!(message.contains(legacy), "{message}");
    assert!(message.contains("moved"), "{message}");
}

#[test]
fn reconcile_unresolved_declared_identity_ignores_a_mismatched_orphan_row() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        record_set_identity_schema()
            .validate()
            .expect("record-set schema fixture must be valid"),
    );
    let mut desired = vec![reference_record_set("new_zone", "new.example.com.", "A")];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("authored hosted-zone reference must have a plan-time address");
    let orphan = anonymous_state_entry(
        &registry,
        "mock",
        "route53.RecordSet",
        "mock_route53_record_set_11111111",
        None,
        None,
        [
            ("hosted_zone_id", concrete_string("Z-OLD")),
            ("name", concrete_string("old.example.com.")),
            ("type", concrete_string("A")),
        ],
    );

    let moves = try_reconcile_anonymous_identifiers(
        &desired,
        &registry,
        &|_provider, _resource_type| vec![orphan.clone()],
        &|_binding| Vec::new(),
    )
    .expect("the differing record name proves this is not a state migration");

    assert!(moves.is_empty());
}

#[test]
fn reconcile_complete_declared_identity_rejects_multiple_legacy_state_matches() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        budget_identity_schema()
            .validate()
            .expect("budget schema fixture must be valid"),
    );
    let mut desired = vec![budget_resource(Some("monthly"), "forecasted")];
    compute_anonymous_identifiers_for_test(&mut desired, &[], &registry, &|_| Vec::new())
        .expect("desired budget identity");
    let destination = desired[0].id.clone();
    let state_entry = |name: &str| {
        let budget = IndexMap::from([
            ("budget_name".to_string(), concrete_string("monthly")),
            ("budget_type".to_string(), concrete_string("COST")),
        ]);
        anonymous_state_entry(
            &registry,
            "mock",
            "budgets.Budget",
            name,
            None,
            None,
            [("budget", Value::Concrete(ConcreteValue::Map(budget)))],
        )
    };
    let state_entries = vec![
        state_entry("mock_budgets_budget_11111111"),
        state_entry("mock_budgets_budget_22222222"),
    ];
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        &registry,
        desired.iter().map(|resource| &resource.id),
    );
    let claims = StateBlockClaims::empty();
    let find_state_by_type = |provider: &str, resource_type: &str| {
        claims.screen_entries(
            provider,
            resource_type,
            state_entries.clone(),
            AnonymousIdStateInfo::name,
        )
    };

    let error = super::reconcile_anonymous_identifiers(
        &desired,
        &[],
        &registry,
        &comparison_provider,
        &find_state_by_type,
        &|_binding| Vec::new(),
        &claims,
    )
    .expect_err("multiple complete declared-identity matches must be rejected");

    let AnonymousIdentifierReconciliationError::AmbiguousDeclaredIdentity(ambiguous) = &error
    else {
        panic!("expected typed ambiguous declared-identity error, got {error}");
    };
    assert_eq!(
        ambiguous.resource().identity_str(),
        destination.identity_str().unwrap()
    );
    assert_eq!(
        ambiguous
            .candidate_rows()
            .iter()
            .map(|candidate| candidate.identity_str())
            .collect::<Vec<_>>(),
        vec![
            "mock_budgets_budget_11111111",
            "mock_budgets_budget_22222222"
        ]
    );
    let message = error.to_string();
    assert!(message.contains(&destination.to_string()), "{message}");
    assert!(
        message.contains("mock_budgets_budget_11111111"),
        "{message}"
    );
    assert!(
        message.contains("mock_budgets_budget_22222222"),
        "{message}"
    );
    assert!(message.contains("moved"), "{message}");
}

#[test]
fn test_reconcile_anonymous_id_full_match_claims_state_entry_once() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route53_record_set_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut resources = vec![
        route53_record_set_resource("A"),
        route53_record_set_resource("AAAA"),
    ];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .expect("identity attrs should produce distinct Route53 record identifiers");

    let old_state_name = "route53_record_set_11223344";
    assert_ne!(
        resources[0].id.identity_str().expect("resolved identity"),
        old_state_name
    );
    assert_ne!(
        resources[1].id.identity_str().expect("resolved identity"),
        old_state_name
    );

    let original_names: Vec<String> = resources
        .iter()
        .map(|resource| {
            resource
                .id
                .identity_str()
                .expect("resolved identity")
                .to_string()
        })
        .collect();
    let state_entries = vec![route53_state_entry(&schemas, old_state_name)];
    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    let names: Vec<String> = resources
        .iter()
        .map(|resource| {
            resource
                .id
                .identity_str()
                .expect("resolved identity")
                .to_string()
        })
        .collect();
    assert_eq!(
        names, original_names,
        "reconcile must not change ResourceIds"
    );
    assert_eq!(
        moves.len(),
        1,
        "only one resource may claim the state entry"
    );
    assert_eq!(moves[0].from().identity_str(), old_state_name);
    assert!(
        original_names
            .iter()
            .any(|name| name == moves[0].to().identity_str()),
        "the claimed state entry must move to one of the desired identities"
    );
}

#[test]
fn test_reconcile_anonymous_id_full_match_does_not_steal_live_entry() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route53_record_set_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut resources = vec![
        route53_record_set_resource("A"),
        route53_record_set_resource("AAAA"),
    ];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .expect("identity attrs should produce distinct Route53 record identifiers");

    let live_a_name = resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let new_aaaa_name = resources[1]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let state_entries = vec![route53_state_entry(&schemas, &live_a_name)];

    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        live_a_name
    );
    assert_eq!(
        resources[1].id.identity_str().expect("resolved identity"),
        new_aaaa_name,
        "new AAAA record must not steal the live A record state entry"
    );
}

#[test]
fn test_simhash_similar_inputs_close_distance() {
    use std::collections::BTreeMap;

    // Two attribute sets differing by one value should have small Hamming distance
    let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
    attrs1.insert("domain", "vpc".to_string());
    attrs1.insert("tag_name", "my-eip".to_string());
    attrs1.insert("tag_env", "production".to_string());
    attrs1.insert("tag_team", "platform".to_string());
    attrs1.insert("region", "ap-northeast-1".to_string());

    let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
    attrs2.insert("domain", "vpc".to_string());
    attrs2.insert("tag_name", "my-eip".to_string());
    attrs2.insert("tag_env", "staging".to_string()); // Only this changed
    attrs2.insert("tag_team", "platform".to_string());
    attrs2.insert("region", "ap-northeast-1".to_string());

    let hash1 = compute_simhash(&attrs1);
    let hash2 = compute_simhash(&attrs2);
    let distance = hash1.distance(hash2);

    // Similar inputs (1 of 5 changed) should have small Hamming distance
    assert!(
        distance < SIMHASH_HAMMING_THRESHOLD,
        "Hamming distance {} should be < {} for similar inputs (1 of 5 attrs changed)",
        distance,
        SIMHASH_HAMMING_THRESHOLD
    );
}

#[test]
fn test_simhash_identical_inputs_zero_distance() {
    use std::collections::BTreeMap;

    let mut attrs: BTreeMap<&str, String> = BTreeMap::new();
    attrs.insert("domain", "vpc".to_string());
    attrs.insert("tag_name", "my-eip".to_string());

    let hash1 = compute_simhash(&attrs);
    let hash2 = compute_simhash(&attrs);
    assert_eq!(hash1, hash2);
}

#[test]
fn test_extract_hash_from_identifier() {
    // 16 hex chars (SimHash, 64-bit)
    assert_eq!(
        extract_hash_from_identifier("ec2_eip_a3f2b1c8d79f1524"),
        Some(AnonymousHashSuffix::SimHash(
            SimHash::parse_16_hex("a3f2b1c8d79f1524").unwrap()
        ))
    );
    // 8 hex chars (standard hash, 32-bit) - still supported
    assert_eq!(
        extract_hash_from_identifier("ec2_vpc_00000000"),
        Some(AnonymousHashSuffix::Standard(0))
    );
    assert_eq!(extract_hash_from_identifier("short"), None);
    assert_eq!(extract_hash_from_identifier("bad_zzzzzzzz"), None);
    // 12 hex chars (neither 8 nor 16) - rejected
    assert_eq!(extract_hash_from_identifier("ec2_eip_aabbccddeeff"), None);
}

#[test]
fn anonymous_identity_discriminator_uses_canonical_hash_suffix_parser() {
    assert_eq!(
        anonymous_identity_discriminator(&ResourceIdentity::new(
            "awscc_ec2_security_group_166e04b3"
        )),
        Some("166e04b3".to_string())
    );
    assert_eq!(
        anonymous_identity_discriminator(&ResourceIdentity::new(
            "module.awscc_ec2_eip_a3f2b1c8d79f1524"
        )),
        Some("a3f2b1c8d79f1524".to_string())
    );
    assert_eq!(
        anonymous_identity_discriminator(&ResourceIdentity::new("named_resource")),
        None
    );
}

fn subnet_route_table_association_schema<R: crate::schema::PatternForm>()
-> crate::schema::PatternResourceSchema<R> {
    PatternResourceSchema::new("ec2.SubnetRouteTableAssociation")
        .attribute(
            PatternAttributeSchema::optional("route_table_id", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("subnet_id", PatternAttributeType::string())
                .create_only()
                .build(),
        )
}

fn subnet_route_table_association_registry() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        subnet_route_table_association_schema()
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas
}

fn subnet_route_table_association_resource(
    name: &'static str,
    route_table_binding: &str,
    subnet_binding: &str,
) -> Resource {
    use crate::resource::AccessPath;

    let mut resource =
        Resource::with_provider("awscc", "ec2.SubnetRouteTableAssociation", name, None);
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root(route_table_binding, "id"),
        }),
    );
    resource.set_attr(
        "subnet_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root(subnet_binding, "id"),
        }),
    );
    resource
}

fn binding_state_entry(
    binding: &str,
    name: &str,
    attrs: &[(&str, &str)],
) -> (String, AnonymousIdBindingStateInfo) {
    (
        binding.to_string(),
        AnonymousIdBindingStateInfo {
            name: name.to_string(),
            attribute_values: attrs
                .iter()
                .map(|(attr, value)| {
                    (
                        (*attr).to_string(),
                        Value::Concrete(ConcreteValue::String((*value).to_string().into())),
                    )
                })
                .collect(),
        },
    )
}

fn binding_state_entries_for(
    entries: &[(String, AnonymousIdBindingStateInfo)],
    binding: &str,
) -> Vec<AnonymousIdBindingStateInfo> {
    entries
        .iter()
        .filter(|(entry_binding, _)| entry_binding == binding)
        .map(|(_, entry)| entry.clone())
        .collect()
}

#[test]
fn empty_struct_create_only_values_are_absent_inline_deferred_and_in_state() {
    let schema = PatternResourceSchema::new("test.Widget")
        .attribute(
            PatternAttributeSchema::optional(
                "config",
                PatternAttributeType::struct_(
                    "Config",
                    vec![
                        PatternStructField::optional("label", PatternAttributeType::string())
                            .build(),
                    ],
                ),
            )
            .create_only()
            .build(),
        )
        .validate()
        .expect("schema fixture must be valid");
    let empty = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
    let inline = Resource::new("test.Widget", "inline").with_attribute("config", empty.clone());
    let deferred = Resource::new("test.Widget", "deferred").with_attribute(
        "config",
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root("source", "config"),
        }),
    );
    let find_state = |binding: &str| {
        (binding == "source")
            .then(|| AnonymousIdBindingStateInfo {
                name: "source".to_string(),
                attribute_values: HashMap::from([("config".to_string(), empty.clone())]),
            })
            .into_iter()
            .collect()
    };
    let state = StateCreateOnlyValues::from_state_attributes(
        &schema,
        &HashMap::from([("config".to_string(), empty.clone())]),
    );

    for desired in [&inline, &deferred] {
        let desired =
            DesiredCreateOnlyValues::from_resource(desired, schema.as_schema(), &find_state);
        assert!(
            !desired.has_configured_values(),
            "an empty struct must be identical to an unconfigured identity input"
        );
        assert_eq!(
            desired.resolved_subset_match(
                &state,
                schema.as_schema(),
                &crate::differ::exact_comparison_context_for_tests(),
            ),
            (0, true),
        );
    }
}

#[test]
fn non_empty_create_only_value_keeps_its_deterministic_identity_string() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let schema = ResourceSchema::new("test.Widget").attribute(
        AttributeSchema::optional(
            "config",
            AttributeType::struct_(
                "Config",
                vec![
                    StructField::optional(
                        "labels",
                        AttributeType::union(vec![
                            AttributeType::string(),
                            AttributeType::list(AttributeType::string()),
                        ]),
                    )
                    .build(),
                ],
            ),
        )
        .create_only()
        .build(),
    );
    let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "labels".to_string(),
        Value::Concrete(ConcreteValue::String("configured".into())),
    )])));
    let resource = Resource::new("test.Widget", "example").with_attribute("config", value.clone());
    let schema = schema.validate().expect("schema fixture must be valid");
    let AnonymousIdentityInputs::CreateOnly(hash_inputs) =
        anonymous_identity_inputs(&resource, &schema)
    else {
        panic!("a schema without declared identity must retain create-only hashing");
    };

    assert_eq!(
        hash_inputs.get("config"),
        Some(&deterministic_value_string(&value)),
        "comparison canonicalization must not alter source-based anonymous identity hashing",
    );
}

fn association_state_entry(
    registry: &SchemaRegistry,
    name: &str,
    route_table_id: &str,
    subnet_id: &str,
) -> AnonymousIdStateInfo {
    anonymous_state_entry(
        registry,
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        name,
        None,
        None,
        [
            ("route_table_id", concrete_string(route_table_id)),
            ("subnet_id", concrete_string(subnet_id)),
        ],
    )
}

fn route_with_deferred_route_table(name: &'static str, route_table_binding: &str) -> Resource {
    use crate::resource::AccessPath;

    let mut resource = Resource::with_provider("awscc", "ec2.Route", name, None);
    resource.module_source = Some(live_module_source("mymod", "inst"));
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root(route_table_binding, "id"),
        }),
    );
    resource
}

fn route_schema_with_create_only_route_table<R: crate::schema::PatternForm>()
-> crate::schema::PatternResourceSchema<R> {
    PatternResourceSchema::new("ec2.Route").attribute(
        PatternAttributeSchema::optional("route_table_id", PatternAttributeType::string())
            .create_only()
            .build(),
    )
}

fn route_state_entry(
    registry: &SchemaRegistry,
    name: &str,
    route_table_id: &str,
) -> AnonymousIdStateInfo {
    anonymous_state_entry(
        registry,
        "awscc",
        "ec2.Route",
        name,
        None,
        None,
        [("route_table_id", concrete_string(route_table_id))],
    )
}

#[test]
fn test_reconcile_does_not_hamming_match_standard_hash_names() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (subnet_route_table_association_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let original_names = [
        "awscc_ec2_subnet_route_table_association_aaaaaaaa",
        "awscc_ec2_subnet_route_table_association_bbbbbbbb",
        "awscc_ec2_subnet_route_table_association_cccccccc",
    ];
    let resources = vec![
        subnet_route_table_association_resource(original_names[0], "private_rtb", "private_subnet"),
        subnet_route_table_association_resource(original_names[1], "db_rtb", "db_subnet"),
        subnet_route_table_association_resource(original_names[2], "public_rtb", "public_subnet"),
    ];

    let state_entries = vec![
        anonymous_state_entry_without_attributes(
            &schemas,
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            "awscc_ec2_subnet_route_table_association_aaaaaaab",
        ),
        anonymous_state_entry_without_attributes(
            &schemas,
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            "awscc_ec2_subnet_route_table_association_bbbbbbba",
        ),
        anonymous_state_entry_without_attributes(
            &schemas,
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            "awscc_ec2_subnet_route_table_association_cccccccd",
        ),
    ];

    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert!(
        moves.is_empty(),
        "8-hex standard hashes must never be used for SimHash Hamming moves"
    );
    assert_eq!(
        resources
            .iter()
            .map(|r| r.id.identity_str().expect("resolved identity"))
            .collect::<Vec<_>>(),
        original_names
    );
}

#[test]
fn test_reconcile_resolves_deferred_create_only_via_state_bindings() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (subnet_route_table_association_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let desired_names = [
        "awscc_ec2_subnet_route_table_association_aaaaaaaa",
        "awscc_ec2_subnet_route_table_association_bbbbbbbb",
        "awscc_ec2_subnet_route_table_association_cccccccc",
    ];
    let state_names = [
        "awscc_ec2_subnet_route_table_association_11111111",
        "awscc_ec2_subnet_route_table_association_22222222",
        "awscc_ec2_subnet_route_table_association_33333333",
    ];
    let resources = vec![
        subnet_route_table_association_resource(desired_names[0], "private_rtb", "private_subnet"),
        subnet_route_table_association_resource(desired_names[1], "db_rtb", "db_subnet"),
        subnet_route_table_association_resource(desired_names[2], "public_rtb", "public_subnet"),
    ];
    let state_entries = vec![
        association_state_entry(&schemas, state_names[0], "rtb-private", "subnet-private"),
        association_state_entry(&schemas, state_names[1], "rtb-db", "subnet-db"),
        association_state_entry(&schemas, state_names[2], "rtb-public", "subnet-public"),
    ];
    let binding_entries = [
        binding_state_entry("private_rtb", "private_rtb", &[("id", "rtb-private")]),
        binding_state_entry(
            "private_subnet",
            "private_subnet",
            &[("id", "subnet-private")],
        ),
        binding_state_entry("db_rtb", "db_rtb", &[("id", "rtb-db")]),
        binding_state_entry("db_subnet", "db_subnet", &[("id", "subnet-db")]),
        binding_state_entry("public_rtb", "public_rtb", &[("id", "rtb-public")]),
        binding_state_entry("public_subnet", "public_subnet", &[("id", "subnet-public")]),
    ];

    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
    );

    assert_eq!(
        resources
            .iter()
            .map(|resource| resource.id.identity_str().expect("resolved identity"))
            .collect::<Vec<_>>(),
        desired_names
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        state_names
            .into_iter()
            .zip(desired_names)
            .map(|(from, to)| (from.to_string(), to.to_string()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_reconcile_skips_state_entry_claimed_by_moved_from() {
    let mut simhash_schemas = SchemaRegistry::new();
    simhash_schemas.insert(
        "awscc",
        simhash_eip_schema()
            .validate()
            .expect("simhash EIP schema must be valid"),
    );
    let simhash_resources = vec![Resource::with_provider(
        "awscc",
        "ec2.eip",
        "awscc_ec2_eip_0000000000000002",
        None,
    )];
    let simhash_state = vec![anonymous_state_entry_without_attributes(
        &simhash_schemas,
        "awscc",
        "ec2.eip",
        "awscc_ec2_eip_0000000000000003",
    )];
    let claims = StateBlockClaims::new(
        [StateBlockAddress::new(
            "awscc",
            "ec2.eip",
            "awscc_ec2_eip_0000000000000003",
        )]
        .into_iter()
        .collect(),
        HashSet::new(),
    );
    let (_, simhash_comparison_provider) = crate::provider::comparison_fixture_for_test(
        &simhash_schemas,
        simhash_resources.iter().map(|resource| &resource.id),
    );

    let simhash_renames = super::reconcile_anonymous_identifiers(
        &simhash_resources,
        &[],
        &simhash_schemas,
        &simhash_comparison_provider,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                simhash_state.clone(),
                AnonymousIdStateInfo::name,
            )
        },
        &|_| Vec::new(),
        &claims,
    )
    .expect("SimHash comparison fixture must resolve");
    assert!(
        simhash_renames.is_empty(),
        "claimed moved.from state row must be excluded from SimHash candidates"
    );

    let mut create_only_schemas = SchemaRegistry::new();
    create_only_schemas.insert(
        "awscc",
        route_schema_with_create_only_route_table()
            .validate()
            .expect("route schema must be valid"),
    );
    let create_only_resources = vec![route_with_deferred_route_table(
        "awscc_ec2_route_aaaaaaaa",
        "private_rtb",
    )];
    let create_only_state = vec![route_state_entry(
        &create_only_schemas,
        "awscc_ec2_route_11111111",
        "rtb-private",
    )];
    let binding_entries = [binding_state_entry(
        "private_rtb",
        "private_rtb",
        &[("id", "rtb-private")],
    )];
    let claims = StateBlockClaims::new(
        [StateBlockAddress::new(
            "awscc",
            "ec2.Route",
            "awscc_ec2_route_11111111",
        )]
        .into_iter()
        .collect(),
        HashSet::new(),
    );
    let (_, create_only_comparison_provider) = crate::provider::comparison_fixture_for_test(
        &create_only_schemas,
        create_only_resources.iter().map(|resource| &resource.id),
    );

    let create_only_renames = super::reconcile_anonymous_identifiers(
        &create_only_resources,
        &[],
        &create_only_schemas,
        &create_only_comparison_provider,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                create_only_state.clone(),
                AnonymousIdStateInfo::name,
            )
        },
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
        &claims,
    )
    .expect("create-only comparison fixture must resolve");
    assert!(create_only_renames.is_empty());
    assert_eq!(
        create_only_resources[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        "awscc_ec2_route_aaaaaaaa",
        "claimed moved.from state row must be excluded from create-only full matches"
    );
}

#[test]
fn test_reconcile_skips_desired_name_claimed_by_moved_to() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route_schema_with_create_only_route_table())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let desired_name = "awscc_ec2_route_aaaaaaaa";
    let resources = vec![route_with_deferred_route_table(desired_name, "private_rtb")];
    let state_entries = vec![route_state_entry(
        &schemas,
        "awscc_ec2_route_11111111",
        "rtb-private",
    )];
    let binding_entries = [binding_state_entry(
        "private_rtb",
        "private_rtb",
        &[("id", "rtb-private")],
    )];
    let claims = StateBlockClaims::new(
        HashSet::new(),
        [StateBlockAddress::new("awscc", "ec2.Route", desired_name)]
            .into_iter()
            .collect(),
    );
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        &schemas,
        resources.iter().map(|resource| &resource.id),
    );

    super::reconcile_anonymous_identifiers(
        &resources,
        &[],
        &schemas,
        &comparison_provider,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                state_entries.clone(),
                AnonymousIdStateInfo::name,
            )
        },
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
        &claims,
    )
    .expect("create-only comparison fixture must resolve");

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        desired_name,
        "desired name claimed as moved.to must skip heuristic reconciliation"
    );
}

#[test]
fn test_reconcile_deferred_create_only_ambiguous_refuses() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (subnet_route_table_association_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let original_name = "awscc_ec2_subnet_route_table_association_aaaaaaaa";
    let resources = vec![subnet_route_table_association_resource(
        original_name,
        "private_rtb",
        "private_subnet",
    )];
    let state_entries = vec![
        association_state_entry(
            &schemas,
            "awscc_ec2_subnet_route_table_association_11111111",
            "rtb-private",
            "subnet-private",
        ),
        association_state_entry(
            &schemas,
            "awscc_ec2_subnet_route_table_association_22222222",
            "rtb-private",
            "subnet-private",
        ),
    ];
    let binding_entries = [
        binding_state_entry("private_rtb", "private_rtb", &[("id", "rtb-private")]),
        binding_state_entry(
            "private_subnet",
            "private_subnet",
            &[("id", "subnet-private")],
        ),
    ];

    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
    );

    assert!(moves.is_empty());
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_name
    );
}

#[test]
fn test_reconcile_deferred_binding_ambiguous_yields_no_value() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route_schema_with_create_only_route_table())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let original_name = "awscc_ec2_route_aaaaaaaa";
    let mut resource = Resource::with_provider("awscc", "ec2.Route", original_name, None);
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: crate::resource::ExpandedAccessPath::root("private_rtb", "id"),
        }),
    );
    let resources = vec![resource];
    let state_entries = vec![route_state_entry(
        &schemas,
        "awscc_ec2_route_11111111",
        "rtb-private",
    )];
    let binding_entries = [
        binding_state_entry("private_rtb", "private_rtb_a", &[("id", "rtb-private")]),
        binding_state_entry("private_rtb", "private_rtb_b", &[("id", "rtb-private")]),
    ];

    let renames = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
    );

    assert!(renames.is_empty());
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_name
    );
}

#[test]
fn test_reconcile_module_argument_passed_root_binding_resolves() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route_schema_with_create_only_route_table())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let desired_name = "inst.awscc_ec2_route_aaaaaaaa";
    let state_name = "inst.awscc_ec2_route_11111111";
    let resources = vec![route_with_deferred_route_table(desired_name, "private_rtb")];
    let state_entries = vec![route_state_entry(&schemas, state_name, "rtb-private")];
    let binding_entries = [binding_state_entry(
        "private_rtb",
        "private_rtb",
        &[("id", "rtb-private")],
    )];

    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
    );

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(state_name.to_string(), desired_name.to_string())]
    );
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        desired_name
    );
}

#[test]
fn test_reconcile_module_intra_module_binding_resolves() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route_schema_with_create_only_route_table())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let desired_name = "inst.awscc_ec2_route_aaaaaaaa";
    let state_name = "inst.awscc_ec2_route_11111111";
    let resources = vec![route_with_deferred_route_table(desired_name, "inst.x")];
    let state_entries = vec![route_state_entry(&schemas, state_name, "rtb-private")];
    let binding_entries = [binding_state_entry(
        "inst.x",
        "inst.x",
        &[("id", "rtb-private")],
    )];

    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
    );

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![(state_name.to_string(), desired_name.to_string())]
    );
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        desired_name
    );
}

#[test]
fn test_reconcile_bare_binding_does_not_resolve_module_prefixed_state_entry() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route_schema_with_create_only_route_table())
            .validate()
            .expect("schema fixture must be valid"),
    );

    let desired_name = "inst.awscc_ec2_route_aaaaaaaa";
    let state_name = "inst.awscc_ec2_route_11111111";
    let resources = vec![route_with_deferred_route_table(desired_name, "private_rtb")];
    let state_entries = vec![route_state_entry(&schemas, state_name, "rtb-private")];
    let binding_entries = [binding_state_entry(
        "private_rtb",
        "inst.private_rtb",
        &[("id", "rtb-private")],
    )];

    let renames = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
    );

    assert!(renames.is_empty());
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        desired_name
    );
}

#[test]
fn test_reconcile_simhash_tie_refused() {
    let schema = PatternResourceSchema::new("ec2.eip").attribute(
        PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let original_name = "awscc_ec2_eip_0000000000000000";
    let resources = vec![Resource::with_provider(
        "awscc",
        "ec2.eip",
        original_name,
        None,
    )];
    let state_entries = vec![
        anonymous_state_entry_without_attributes(
            &schemas,
            "awscc",
            "ec2.eip",
            "awscc_ec2_eip_0000000000000001",
        ),
        anonymous_state_entry_without_attributes(
            &schemas,
            "awscc",
            "ec2.eip",
            "awscc_ec2_eip_0000000000000002",
        ),
    ];

    let renames = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert!(renames.is_empty());
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_name
    );
}

#[test]
fn reconcile_create_only_match_tracks_origin_and_projected_source_separately() {
    let schema = PatternResourceSchema::new("iam.role").attribute(
        PatternAttributeSchema::optional("role_name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let desired_name = "awscc_iam_role_33333333";
    let resource = Resource::with_provider("awscc", "iam.role", desired_name, None)
        .with_attribute("role_name", concrete_string("app"));
    let state_entry = anonymous_state_entry(
        &schemas,
        "awscc",
        "iam.role",
        "legacy_iam_role",
        Some("awscc_iam_role_22222222"),
        None,
        [("role_name", concrete_string("app"))],
    );

    let moves = reconcile_anonymous_identifiers(
        &[resource],
        &schemas,
        &|_provider, _resource_type| vec![state_entry.clone()],
        &|_binding| Vec::new(),
    );

    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].from().identity_str(), "legacy_iam_role");
    assert_eq!(
        moves[0].projected_from().identity_str(),
        "awscc_iam_role_22222222"
    );
    assert_eq!(moves[0].to().identity_str(), desired_name);
}

#[test]
fn reconcile_simhash_match_tracks_origin_and_projected_source_separately() {
    let schema = PatternResourceSchema::new("ec2.eip").attribute(
        PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let desired_name = "awscc_ec2_eip_0000000000000000";
    let resource = Resource::with_provider("awscc", "ec2.eip", desired_name, None);
    let state_entry = anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.eip",
        "legacy_eip",
        Some("awscc_ec2_eip_0000000000000001"),
        None,
        std::iter::empty::<(&str, Value)>(),
    );

    let moves = reconcile_anonymous_identifiers(
        &[resource],
        &schemas,
        &|_provider, _resource_type| vec![state_entry.clone()],
        &|_binding| Vec::new(),
    );

    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].from().identity_str(), "legacy_eip");
    assert_eq!(
        moves[0].projected_from().identity_str(),
        "awscc_ec2_eip_0000000000000001"
    );
    assert_eq!(moves[0].to().identity_str(), desired_name);
}

#[test]
fn test_canonical_create_only_string_covers_concrete_scalars() {
    assert_eq!(
        canonical_create_only_value_string_for_test(&Value::Concrete(ConcreteValue::Int(42))),
        Some("Int(42)".to_string())
    );
    assert_eq!(
        canonical_create_only_value_string_for_test(&Value::Concrete(ConcreteValue::Float(3.5))),
        Some("Float(3.5)".to_string())
    );
    assert_eq!(
        canonical_create_only_value_string_for_test(&Value::Concrete(ConcreteValue::Bool(true))),
        Some("Bool(true)".to_string())
    );
    assert_eq!(
        canonical_create_only_value_string_for_test(&Value::Concrete(ConcreteValue::Duration(
            std::time::Duration::from_secs(90)
        ))),
        Some("Int(90)".to_string())
    );
    assert_eq!(
        canonical_create_only_value_string_for_test(&Value::Concrete(ConcreteValue::StringList(
            vec!["alpha".into(), "beta".into(),]
        ))),
        Some("List([alpha, beta])".to_string())
    );
}

#[test]
fn canonical_enum_create_only_comparison_preserves_identity_hash_material() {
    let value = Value::Concrete(ConcreteValue::CanonicalEnum(canonical_eip_domain(
        "awscc",
        "awscc.ec2.Eip.Domain.vpc",
    )));

    assert_eq!(
        deterministic_value_string(&value),
        r#"EnumApiValue("vpc")"#,
        "canonical create-only comparison changes must not alter anonymous-id hash input",
    );
    assert_eq!(
        canonical_create_only_value_string_for_test(&value),
        Some(r#"EnumApiValue("vpc")"#.to_string()),
    );
}

#[test]
fn upstream_unknown_identity_hash_strings_remain_stable() {
    let export = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
        path: AccessPath::root("network", "vpc_id"),
    }));
    assert_eq!(
        deterministic_value_string(&export),
        "Unknown(UpstreamRef(network.vpc_id))"
    );

    let all_exports = Value::Deferred(DeferredValue::Unknown(
        ApplyTimeUnknown::UpstreamAllExports {
            binding: absolute_binding("network"),
        },
    ));
    assert_eq!(
        deterministic_value_string(&all_exports),
        "Unknown(UpstreamBareRef(network))"
    );
}

#[test]
fn test_canonical_create_only_string_matches_state_round_trip() {
    let values = {
        let mut map = IndexMap::new();
        map.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("alpha".into())),
        );
        map.insert("count".to_string(), Value::Concrete(ConcreteValue::Int(2)));

        vec![
            Value::Concrete(ConcreteValue::String("alpha".into())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Float(3.5)),
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(300))),
            Value::Concrete(ConcreteValue::StringList(vec![
                "alpha".into(),
                "beta".into(),
            ])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("alpha".into())),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::StringList(vec!["alpha".into(), "beta".into()]),
            )])),
            Value::Concrete(ConcreteValue::Map(map)),
        ]
    };

    for value in values {
        let state_json = crate::value::PersistedValue::encode_provider_value(&value, None)
            .expect("value should serialize for persisted state");
        let persisted = state_json
            .decode()
            .expect("persisted state value should decode");
        assert_eq!(
            canonical_create_only_value_string_for_test(&value),
            canonical_create_only_value_string_for_test(&persisted),
            "round-trip canonical mismatch for {value:?}",
        );
    }
}

#[test]
fn test_reconcile_anonymous_id_no_create_only_hamming_match() {
    // When schema has no create-only properties and an attribute changes,
    // Hamming distance reconciliation should match with the closest state entry.
    let schema = PatternResourceSchema::new("ec2.eip")
        .attribute(
            PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_name", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_env", PatternAttributeType::string()).build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: vec![(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        )]
        .into_iter()
        .collect(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

    // Step 1: compute identifier with tag_env="production"
    let mut r1 = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r1.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    r1.set_attr(
        "tag_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-eip".into())),
    );
    r1.set_attr(
        "tag_env".to_string(),
        Value::Concrete(ConcreteValue::String("production".into())),
    );
    let mut resources1 = vec![r1];
    compute_anonymous_identifiers_for_test(&mut resources1, &providers, &schemas, &identity_fn)
        .unwrap();
    let old_id = resources1[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Step 2: compute identifier with tag_env="staging" (one attribute changed)
    let mut r2 = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r2.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    r2.set_attr(
        "tag_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-eip".into())),
    );
    r2.set_attr(
        "tag_env".to_string(),
        Value::Concrete(ConcreteValue::String("staging".into())),
    );
    let mut resources2 = vec![r2];
    compute_anonymous_identifiers_for_test(&mut resources2, &providers, &schemas, &identity_fn)
        .unwrap();
    let new_id = resources2[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Identifiers should differ (different attributes)
    assert_ne!(old_id, new_id);

    // Reconcile: state has the old identifier
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas, "awscc", "ec2.eip", &old_id,
    )];
    let renames = reconcile_anonymous_identifiers(
        &resources2,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // After reconciliation: the resource keeps its freshly-computed
    // (new) identifier and a move is emitted so the wiring
    // layer can re-key the state entry. Previous behavior (overwriting
    // resource.id with the state's old name) was changed when the
    // provider prefix was introduced — the rename path is now the
    // only mechanism for legacy state name reuse.
    assert_eq!(
        resources2[0].id.identity_str().expect("resolved identity"),
        new_id
    );
    assert_eq!(
        resolved_move_identity_pairs(&renames),
        vec![(old_id.clone(), new_id.clone())]
    );
}

#[test]
fn test_reconcile_anonymous_id_simhash_does_not_steal_live_entry() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (simhash_eip_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut prior_resources = vec![simhash_eip_resource("production")];
    compute_anonymous_identifiers_for_test(
        &mut prior_resources,
        &providers,
        &schemas,
        &identity_fn,
    )
    .unwrap();
    let live_name = prior_resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // The current configuration has one named owner of the live row and one
    // anonymous resource. Two attribute-derived anonymous declarations of the
    // same kind are rejected before reconciliation in production.
    let mut live_resource = prior_resources.remove(0);
    live_resource.binding = Some(absolute_binding(&live_name));
    let mut resources = vec![live_resource, simhash_eip_resource("staging")];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();
    let new_name = resources[1]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas, "awscc", "ec2.eip", &live_name,
    )];
    let renames = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        live_name
    );
    assert_eq!(
        resources[1].id.identity_str().expect("resolved identity"),
        new_name
    );
    assert!(
        renames.is_empty(),
        "SimHash reconcile must not rename a live state row already used by another resource"
    );
}

#[test]
fn test_reconcile_anonymous_id_simhash_claims_state_entry_once() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (simhash_eip_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let providers = vec![provider_config("awscc", vec![])];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut old_resources = vec![simhash_eip_resource("production")];
    compute_anonymous_identifiers_for_test(&mut old_resources, &providers, &schemas, &identity_fn)
        .unwrap();
    let old_name = old_resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let old_hash = simhash_suffix_for_test(&old_name);

    let mut nearby_resources: Vec<Resource> = Vec::new();
    for tag_env in [
        "staging",
        "prod",
        "development",
        "qa",
        "test",
        "preview",
        "canary",
    ] {
        let mut candidate = vec![simhash_eip_resource(tag_env)];
        compute_anonymous_identifiers_for_test(&mut candidate, &providers, &schemas, &identity_fn)
            .unwrap();
        let candidate_hash =
            simhash_suffix_for_test(candidate[0].id.identity_str().expect("resolved identity"));
        if candidate[0].id.identity_str().expect("resolved identity") != old_name
            && old_hash.distance(candidate_hash) < SIMHASH_HAMMING_THRESHOLD
        {
            nearby_resources.push(candidate.remove(0));
        }
        if nearby_resources.len() == 2 {
            break;
        }
    }
    assert_eq!(
        nearby_resources.len(),
        2,
        "test setup should find two distinct resources near the same old SimHash"
    );

    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas, "awscc", "ec2.eip", &old_name,
    )];
    let renames = reconcile_anonymous_identifiers(
        &nearby_resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        renames.len(),
        1,
        "only one resource may claim a SimHash-matched state row"
    );
    assert_eq!(renames[0].from().identity_str(), old_name);
}

#[test]
fn test_reconcile_anonymous_id_no_create_only_no_match_when_distant() {
    // Completely different resources should not reconcile
    let schema = PatternResourceSchema::new("ec2.eip").attribute(
        PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    // Resource with a computed identifier
    let mut resource =
        Resource::with_provider("awscc", "ec2.eip", "ec2_eip_aabbccdd11223344", None);
    resource.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );

    let original_id = resource
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let resources = vec![resource];

    // State has a very different hash (flipped many bits)
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas,
        "awscc",
        "ec2.eip",
        "ec2_eip_5544332266778899",
    )];
    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Identifier should remain unchanged (too distant)
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_id
    );
}

#[test]
fn test_reconcile_anonymous_id_create_only_exists_but_none_set() {
    // Case A: Schema has create-only properties, but user didn't set any.
    // Should use SimHash-based Hamming distance reconciliation.
    let schema = PatternResourceSchema::new("ec2.eip")
        .attribute(
            PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("public_ipv4_pool", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    // Compute identifier without setting the create-only property
    let mut r1 = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r1.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    let mut resources = vec![r1];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();

    // Should have computed an identifier (not errored)
    assert!(
        !resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
            .is_empty()
    );
    assert!(
        resources[0]
            .id
            .identity_str()
            .unwrap_or("")
            .starts_with("awscc_ec2_eip_")
    );

    // Reconciliation should use Hamming distance (create-only values empty)
    let current_id = resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let state_id = current_id.clone(); // Same id in state = no reconciliation needed
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas, "awscc", "ec2.eip", &state_id,
    )];
    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Same identifier in state, no change needed
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        current_id
    );
}

// ==================== SimHash acceptance tests ====================
// Comprehensive tests to verify SimHash behavior across various scenarios.

#[test]
fn test_simhash_different_attribute_count_produces_different_hash() {
    use std::collections::BTreeMap;

    let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
    attrs1.insert("domain", "vpc".to_string());

    let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
    attrs2.insert("domain", "vpc".to_string());
    attrs2.insert("tag_name", "extra".to_string());

    let hash1 = compute_simhash(&attrs1);
    let hash2 = compute_simhash(&attrs2);
    assert_ne!(hash1, hash2, "Adding an attribute should change the hash");
}

#[test]
fn test_simhash_key_change_produces_different_hash() {
    use std::collections::BTreeMap;

    // Same value but different key should produce different hash
    let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
    attrs1.insert("domain", "vpc".to_string());

    let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
    attrs2.insert("region", "vpc".to_string());

    let hash1 = compute_simhash(&attrs1);
    let hash2 = compute_simhash(&attrs2);
    assert_ne!(
        hash1, hash2,
        "Different keys should produce different hashes"
    );
}

#[test]
fn test_simhash_order_independent() {
    use std::collections::BTreeMap;

    // BTreeMap is sorted, so insertion order doesn't matter.
    // Verify that the same key-value pairs produce the same hash regardless.
    let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
    attrs1.insert("a", "1".to_string());
    attrs1.insert("b", "2".to_string());
    attrs1.insert("c", "3".to_string());

    let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
    attrs2.insert("c", "3".to_string());
    attrs2.insert("a", "1".to_string());
    attrs2.insert("b", "2".to_string());

    assert_eq!(compute_simhash(&attrs1), compute_simhash(&attrs2));
}

#[test]
fn test_simhash_empty_attributes() {
    use std::collections::BTreeMap;

    let attrs: BTreeMap<&str, String> = BTreeMap::new();
    // Empty attributes should produce 0 (all vote counters remain 0, all bits off)
    assert!(compute_simhash(&attrs).is_zero_for_test());
}

#[test]
fn test_simhash_single_attribute() {
    use std::collections::BTreeMap;

    let mut attrs: BTreeMap<&str, String> = BTreeMap::new();
    attrs.insert("domain", "vpc".to_string());

    let hash = compute_simhash(&attrs);
    // Single attribute: hash should be non-zero and deterministic
    assert!(!hash.is_zero_for_test());
    assert_eq!(hash, compute_simhash(&attrs));
}

#[test]
fn test_simhash_many_attributes_one_change_close_distance() {
    use std::collections::BTreeMap;

    // With many attributes, changing one should flip very few bits
    let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
    for i in 0..10 {
        attrs1.insert(
            Box::leak(format!("attr_{}", i).into_boxed_str()),
            format!("value_{}", i),
        );
    }

    let mut attrs2 = attrs1.clone();
    attrs2.insert("attr_5", "changed_value".to_string());

    let hash1 = compute_simhash(&attrs1);
    let hash2 = compute_simhash(&attrs2);
    let distance = hash1.distance(hash2);

    assert!(
        distance < SIMHASH_HAMMING_THRESHOLD,
        "Changing 1 of 10 attributes: Hamming distance {} should be < {}",
        distance,
        SIMHASH_HAMMING_THRESHOLD
    );
}

#[test]
fn test_simhash_all_attributes_changed_large_distance() {
    use std::collections::BTreeMap;

    // Completely different attribute values should have large Hamming distance
    let mut attrs1: BTreeMap<&str, String> = BTreeMap::new();
    attrs1.insert("a", "alpha".to_string());
    attrs1.insert("b", "bravo".to_string());
    attrs1.insert("c", "charlie".to_string());
    attrs1.insert("d", "delta".to_string());
    attrs1.insert("e", "echo".to_string());

    let mut attrs2: BTreeMap<&str, String> = BTreeMap::new();
    attrs2.insert("a", "xray".to_string());
    attrs2.insert("b", "yankee".to_string());
    attrs2.insert("c", "zulu".to_string());
    attrs2.insert("d", "foxtrot".to_string());
    attrs2.insert("e", "golf".to_string());

    let hash1 = compute_simhash(&attrs1);
    let hash2 = compute_simhash(&attrs2);

    // All values changed: hashes should differ
    assert_ne!(
        hash1, hash2,
        "Completely different values should produce different hashes"
    );
}

#[test]
fn test_reconcile_no_create_only_picks_closest_among_multiple_state_entries() {
    // When multiple state entries exist, reconciliation should pick the closest one
    let schema = PatternResourceSchema::new("ec2.eip")
        .attribute(
            PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_name", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_env", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_team", PatternAttributeType::string()).build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    // Compute 3 identifiers with different attributes
    let make_resource = |env: &str, team: &str| {
        let mut r = Resource::pending_with_provider("awscc", "ec2.eip", None);
        r.set_attr(
            "domain".to_string(),
            Value::Concrete(ConcreteValue::String("vpc".into())),
        );
        r.set_attr(
            "tag_name".to_string(),
            Value::Concrete(ConcreteValue::String("my-eip".into())),
        );
        r.set_attr(
            "tag_env".to_string(),
            Value::Concrete(ConcreteValue::String(env.to_string().into())),
        );
        r.set_attr(
            "tag_team".to_string(),
            Value::Concrete(ConcreteValue::String(team.to_string().into())),
        );
        r
    };

    // Original: env=prod, team=infra
    let mut resources_orig = vec![make_resource("production", "infra")];
    compute_anonymous_identifiers_for_test(&mut resources_orig, &providers, &schemas, &identity_fn)
        .unwrap();
    let orig_id = resources_orig[0]
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();

    // Distant: env=dev, team=frontend (2 attrs changed)
    let mut resources_distant = vec![make_resource("development", "frontend")];
    compute_anonymous_identifiers_for_test(
        &mut resources_distant,
        &providers,
        &schemas,
        &identity_fn,
    )
    .unwrap();
    let distant_id = resources_distant[0]
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();

    // Current: env=staging, team=infra (1 attr changed from orig)
    let mut resources_current = vec![make_resource("staging", "infra")];
    compute_anonymous_identifiers_for_test(
        &mut resources_current,
        &providers,
        &schemas,
        &identity_fn,
    )
    .unwrap();

    // State has both orig and distant entries
    let state_entries = vec![
        anonymous_state_entry_without_attributes(&schemas, "awscc", "ec2.eip", &orig_id),
        anonymous_state_entry_without_attributes(&schemas, "awscc", "ec2.eip", &distant_id),
    ];

    let current_id_before = resources_current[0]
        .id
        .identity_str()
        .unwrap_or("")
        .to_string();
    let renames = reconcile_anonymous_identifiers(
        &resources_current,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Resource keeps its freshly-computed identifier; the move (if any)
    // points from the closest state entry to the new identifier so the wiring
    // layer can re-key the legacy state row.
    assert_eq!(
        resources_current[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        current_id_before,
        "Resource should retain its freshly-computed identifier",
    );

    let current_hash = simhash_suffix_for_test(&current_id_before);
    let orig_hash = simhash_suffix_for_test(&orig_id);
    let distant_hash = simhash_suffix_for_test(&distant_id);
    let dist_to_orig = current_hash.distance(orig_hash);
    let dist_to_distant = current_hash.distance(distant_hash);

    if dist_to_orig < SIMHASH_HAMMING_THRESHOLD && dist_to_orig <= dist_to_distant {
        // Orig is within threshold and at least as close as distant: the move
        // should reference orig as the source name.
        assert_eq!(
            resolved_move_identity_pairs(&renames),
            vec![(orig_id.clone(), current_id_before.clone())],
            "Closest state entry (orig) should drive the move",
        );
    }
    if dist_to_orig < dist_to_distant {
        assert!(
            dist_to_orig < dist_to_distant,
            "1-attr change (dist={}) should be closer than 2-attr change (dist={})",
            dist_to_orig,
            dist_to_distant,
        );
    }
}

#[test]
fn test_reconcile_no_create_only_same_id_in_state_no_change() {
    // If state already has the same identifier, no reconciliation needed
    let schema = PatternResourceSchema::new("ec2.eip").attribute(
        PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut r = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    let mut resources = vec![r];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();
    let id = resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // State has the exact same identifier
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas, "awscc", "ec2.eip", &id,
    )];
    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Should remain unchanged
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        id
    );
}

#[test]
fn test_reconcile_no_create_only_empty_state() {
    // No state entries = no reconciliation
    let schema = PatternResourceSchema::new("ec2.eip").attribute(
        PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut resource =
        Resource::with_provider("awscc", "ec2.eip", "ec2_eip_aabbccdd11223344", None);
    resource.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    let original_id = resource
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let resources = vec![resource];

    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| vec![],
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_id
    );
}

#[test]
fn test_compute_anonymous_id_uses_simhash_for_no_create_only() {
    // Verify that changing one attribute produces a different but nearby identifier
    let schema = PatternResourceSchema::new("ec2.internet_gateway")
        .attribute(
            PatternAttributeSchema::optional("tag_name", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_env", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_team", PatternAttributeType::string()).build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_resource = |env: &str| {
        let mut r = Resource::pending_with_provider("awscc", "ec2.internet_gateway", None);
        r.set_attr(
            "tag_name".to_string(),
            Value::Concrete(ConcreteValue::String("my-igw".into())),
        );
        r.set_attr(
            "tag_env".to_string(),
            Value::Concrete(ConcreteValue::String(env.to_string().into())),
        );
        r.set_attr(
            "tag_team".to_string(),
            Value::Concrete(ConcreteValue::String("platform".into())),
        );
        r
    };

    let mut r1 = vec![make_resource("production")];
    let mut r2 = vec![make_resource("staging")];
    compute_anonymous_identifiers_for_test(&mut r1, &providers, &schemas, &identity_fn).unwrap();
    compute_anonymous_identifiers_for_test(&mut r2, &providers, &schemas, &identity_fn).unwrap();

    // Different identifiers
    assert_ne!(
        r1[0].id.identity_str().expect("resolved identity"),
        r2[0].id.identity_str().expect("resolved identity")
    );

    // But nearby (SimHash locality-sensitive property)
    let hash1 = simhash_suffix_for_test(r1[0].id.identity_str().expect("resolved identity"));
    let hash2 = simhash_suffix_for_test(r2[0].id.identity_str().expect("resolved identity"));
    let distance = hash1.distance(hash2);
    assert!(
        distance < SIMHASH_HAMMING_THRESHOLD,
        "Single attribute change should produce close SimHash (distance={}, threshold={})",
        distance,
        SIMHASH_HAMMING_THRESHOLD,
    );
}

#[test]
fn test_compute_anonymous_id_simhash_vs_create_only_hash_independent() {
    // Resources with create-only properties use standard hash,
    // resources without use SimHash. Verify both work side by side.
    let schema_with_co = PatternResourceSchema::new("ec2.Vpc")
        .attribute(
            PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_name", PatternAttributeType::string()).build(),
        );
    let schema_without_co = PatternResourceSchema::new("ec2.eip")
        .attribute(
            PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("tag_name", PatternAttributeType::string()).build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema_with_co
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        schema_without_co
            .validate()
            .expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut vpc = Resource::pending_with_provider("awscc", "ec2.Vpc", None);
    vpc.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    vpc.set_attr(
        "tag_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-vpc".into())),
    );

    let mut eip = Resource::pending_with_provider("awscc", "ec2.eip", None);
    eip.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    eip.set_attr(
        "tag_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-eip".into())),
    );

    let mut resources = vec![vpc, eip];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();

    // Both should have identifiers computed
    assert!(
        resources[0]
            .id
            .identity_str()
            .unwrap_or("")
            .starts_with("awscc_ec2_vpc_")
    );
    assert!(
        resources[1]
            .id
            .identity_str()
            .unwrap_or("")
            .starts_with("awscc_ec2_eip_")
    );

    // VPC uses standard hash (8 hex chars), EIP uses SimHash (16 hex chars)
    let vpc_hash_part = resources[0]
        .id
        .identity_str()
        .unwrap_or("")
        .rsplit('_')
        .next()
        .unwrap();
    let eip_hash_part = resources[1]
        .id
        .identity_str()
        .unwrap_or("")
        .rsplit('_')
        .next()
        .unwrap();
    assert_eq!(vpc_hash_part.len(), 8);
    assert_eq!(eip_hash_part.len(), 16);
}

#[test]
fn test_reconcile_create_only_path_unaffected_by_simhash_changes() {
    // Verify that resources WITH create-only properties still use the
    // existing partial-match reconciliation, not Hamming distance.
    let schema = PatternResourceSchema::new("iam.role")
        .attribute(
            PatternAttributeSchema::optional("role_name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("path", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    // Resource with both create-only props set
    let mut resource = Resource::with_provider("awscc", "iam.role", "iam_role_aabbccdd", None);
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-role".into())),
    );
    resource.set_attr(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String("/new/".into())),
    );

    let original_id = resource
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let resources = vec![resource];

    // State with partial match (role_name matches, path differs)
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "iam.role",
        "iam_role_11223344",
        None,
        None,
        [
            ("role_name", concrete_string("my-role")),
            ("path", concrete_string("/old/")),
        ],
    )];

    let moves = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Should reconcile via a partial create-only move (not Hamming distance).
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_id
    );
    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![("iam_role_11223344".to_string(), original_id)]
    );
}

#[test]
fn test_compute_anonymous_id_stable_with_prefixed_create_only_attribute() {
    // When a create-only attribute has a prefix (e.g., bucket_name_prefix),
    // the anonymous identifier should be based on the prefix, not the
    // randomly generated name. This ensures the hash is stable across runs.
    let schema = PatternResourceSchema::new("s3.Bucket").attribute(
        PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    // Simulate two runs with different random suffixes but same prefix
    let make_resource = |generated_name: &str| {
        let mut r = Resource::pending_with_provider("awscc", "s3.Bucket", None);
        r.set_attr(
            "bucket_name".to_string(),
            Value::Concrete(ConcreteValue::String(generated_name.to_string().into())),
        );
        r.prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());
        r
    };

    let mut r1 = vec![make_resource("my-app-abc12345")];
    let mut r2 = vec![make_resource("my-app-xyz98765")];
    compute_anonymous_identifiers_for_test(&mut r1, &providers, &schemas, &identity_fn).unwrap();
    compute_anonymous_identifiers_for_test(&mut r2, &providers, &schemas, &identity_fn).unwrap();

    // Same prefix should produce the same anonymous identifier
    assert_eq!(
        r1[0].id.identity_str().expect("resolved identity"),
        r2[0].id.identity_str().expect("resolved identity"),
        "Prefixed create-only attributes should produce stable identifiers"
    );
}

#[test]
fn test_compute_anonymous_id_different_prefix_produces_different_id() {
    // Different prefixes should produce different anonymous identifiers
    let schema = PatternResourceSchema::new("s3.Bucket").attribute(
        PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let make_resource = |prefix: &str, generated_name: &str| {
        let mut r = Resource::pending_with_provider("awscc", "s3.Bucket", None);
        r.set_attr(
            "bucket_name".to_string(),
            Value::Concrete(ConcreteValue::String(generated_name.to_string().into())),
        );
        r.prefixes
            .insert("bucket_name".to_string(), prefix.to_string());
        r
    };

    let mut r1 = vec![make_resource("app-a-", "app-a-abc12345")];
    let mut r2 = vec![make_resource("app-b-", "app-b-xyz98765")];
    compute_anonymous_identifiers_for_test(&mut r1, &providers, &schemas, &identity_fn).unwrap();
    compute_anonymous_identifiers_for_test(&mut r2, &providers, &schemas, &identity_fn).unwrap();

    // Different prefixes should produce different identifiers
    assert_ne!(
        r1[0].id.identity_str().expect("resolved identity"),
        r2[0].id.identity_str().expect("resolved identity"),
        "Different prefixes should produce different identifiers"
    );
}

#[test]
fn test_reconcile_skips_let_bound_resources() {
    // Let-bound (named) resources should never be reconciled, even if their
    // name doesn't exist in state. The typed binding marks them as named.
    let schema = PatternResourceSchema::new("ec2.security_group_ingress")
        .attribute(
            PatternAttributeSchema::optional("cidr_ip", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("ip_protocol", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("description", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );

    // A let-bound resource whose name does NOT exist in state
    let mut ingress_new =
        Resource::with_provider("aws", "ec2.security_group_ingress", "ingress_new", None);
    ingress_new.binding = Some(absolute_binding("ingress_new"));
    ingress_new.set_attr(
        "cidr_ip".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
    );
    ingress_new.set_attr(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    ingress_new.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("Allow HTTPS".into())),
    );

    let resources = vec![ingress_new];

    // State has an unrelated entry that partially matches (same cidr_ip + ip_protocol,
    // different description). Without the fix, the named resource would be rebound.
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "aws",
        "ec2.security_group_ingress",
        "ec2_security_group_ingress_aabb1122",
        None,
        None,
        [
            ("cidr_ip", concrete_string("0.0.0.0/0")),
            ("ip_protocol", concrete_string("tcp")),
            ("description", concrete_string("Allow HTTP")),
        ],
    )];

    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Named resource must keep its original name
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        "ingress_new",
        "let-bound resource should not be reconciled"
    );
}

#[test]
fn test_reconcile_skips_when_multiple_partial_matches() {
    // When multiple state entries partially match an anonymous resource,
    // reconciliation should skip rather than picking the first match.
    // This prevents a new SG rule from hijacking an unrelated state entry.
    let schema = PatternResourceSchema::new("ec2.security_group_ingress")
        .attribute(
            PatternAttributeSchema::optional("cidr_ip", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("ip_protocol", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("description", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );

    // Anonymous resource with a new hash-derived identifier
    let mut new_rule = Resource::with_provider(
        "aws",
        "ec2.security_group_ingress",
        "ec2_security_group_ingress_deadbeef",
        None,
    );
    new_rule.set_attr(
        "cidr_ip".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
    );
    new_rule.set_attr(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    new_rule.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("Allow gRPC".into())),
    );

    let original_id = new_rule
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    let resources = vec![new_rule];

    // State has TWO entries that partially match (same cidr_ip + ip_protocol,
    // different description). Both are valid partial matches.
    let state_entries = vec![
        anonymous_state_entry(
            &schemas,
            "aws",
            "ec2.security_group_ingress",
            "ec2_security_group_ingress_aabb1122",
            None,
            None,
            [
                ("cidr_ip", concrete_string("0.0.0.0/0")),
                ("ip_protocol", concrete_string("tcp")),
                ("description", concrete_string("Allow HTTP")),
            ],
        ),
        anonymous_state_entry(
            &schemas,
            "aws",
            "ec2.security_group_ingress",
            "ec2_security_group_ingress_ccdd3344",
            None,
            None,
            [
                ("cidr_ip", concrete_string("0.0.0.0/0")),
                ("ip_protocol", concrete_string("tcp")),
                ("description", concrete_string("Allow HTTPS")),
            ],
        ),
    ];

    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // With multiple partial matches, reconciliation should be skipped
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        original_id,
        "ambiguous partial matches should not reconcile"
    );
}

#[test]
fn test_reconcile_eip_tag_update_with_unset_create_only_props() {
    // Regression test for #882: EC2 EIP has create-only props in schema
    // (address, ipam_pool_id, etc.) but user didn't set any. Only tags changed.
    // SimHash reconciliation should match the resource as an in-place update,
    // not a replace (delete+create).
    let schema = PatternResourceSchema::new("ec2.eip")
        .attribute(
            PatternAttributeSchema::optional("domain", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("address", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("ipam_pool_id", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional(
                "network_border_group",
                PatternAttributeType::string(),
            )
            .create_only()
            .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("transfer_address", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional(
                "tags",
                PatternAttributeType::map(PatternAttributeType::string()),
            )
            .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: vec![(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("awscc.Region.ap_northeast_1".into())),
        )]
        .into_iter()
        .collect(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec!["region".to_string()] };

    // Step 1: Create EIP with tags Environment=acceptance-test
    let mut r1 = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r1.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    let mut tags1 = indexmap::IndexMap::new();
    tags1.insert(
        "Environment".to_string(),
        Value::Concrete(ConcreteValue::String("acceptance-test".into())),
    );
    tags1.insert(
        "Purpose".to_string(),
        Value::Concrete(ConcreteValue::String("simhash-test".into())),
    );
    r1.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags1)),
    );

    let mut resources1 = vec![r1];
    compute_anonymous_identifiers_for_test(&mut resources1, &providers, &schemas, &identity_fn)
        .unwrap();
    let step1_id = resources1[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Step 2: Change tag Environment=staging (only tags changed)
    let mut r2 = Resource::pending_with_provider("awscc", "ec2.eip", None);
    r2.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::String("vpc".into())),
    );
    let mut tags2 = indexmap::IndexMap::new();
    tags2.insert(
        "Environment".to_string(),
        Value::Concrete(ConcreteValue::String("staging".into())),
    );
    tags2.insert(
        "Purpose".to_string(),
        Value::Concrete(ConcreteValue::String("simhash-test".into())),
    );
    r2.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags2)),
    );

    let mut resources2 = vec![r2];
    compute_anonymous_identifiers_for_test(&mut resources2, &providers, &schemas, &identity_fn)
        .unwrap();
    let step2_id = resources2[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Identifiers should differ (different tag values)
    assert_ne!(step1_id, step2_id);

    // Reconcile: state has the step1 identifier
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas, "awscc", "ec2.eip", &step1_id,
    )];
    let renames = reconcile_anonymous_identifiers(
        &resources2,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // After reconciliation, step2 keeps its freshly-computed identifier
    // and emits a move so the wiring layer re-keys the state
    // entry from step1's name to step2's name. This produces an
    // in-place update on the same state row instead of delete+create.
    assert_eq!(
        resources2[0].id.identity_str().expect("resolved identity"),
        step2_id,
        "Resource should retain its freshly-computed identifier"
    );
    assert_eq!(
        resolved_move_identity_pairs(&renames),
        vec![(step1_id.clone(), step2_id.clone())],
        "Reconcile should emit a move for re-keying the state entry"
    );
}

#[test]
fn test_reconcile_does_not_swap_named_resources_with_overlapping_create_only() {
    // Regression test for #788: two security_group_ingress rules on the same SG
    // should not be swapped by reconciliation when they share some create-only
    // attributes (cidr_ip, ip_protocol) but differ on others (description, from_port).
    //
    // Both resources are named (let-bound) and already match state entries by name.
    // Reconciliation should leave them unchanged.
    let schema = PatternResourceSchema::new("ec2.security_group_ingress")
        .attribute(
            PatternAttributeSchema::optional("cidr_ip", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("ip_protocol", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("description", PatternAttributeType::string())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );

    // Two named ingress resources with overlapping create-only attributes
    let mut ingress_http =
        Resource::with_provider("aws", "ec2.security_group_ingress", "ingress_http", None);
    ingress_http.set_attr(
        "cidr_ip".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
    );
    ingress_http.set_attr(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    ingress_http.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("Allow HTTP".into())),
    );

    let mut ingress_https =
        Resource::with_provider("aws", "ec2.security_group_ingress", "ingress_https", None);
    ingress_https.set_attr(
        "cidr_ip".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".into())),
    );
    ingress_https.set_attr(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    ingress_https.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("Allow HTTPS".into())),
    );

    let resources = vec![ingress_http, ingress_https];

    // State has both resources with matching names
    let state_entries = vec![
        anonymous_state_entry(
            &schemas,
            "aws",
            "ec2.security_group_ingress",
            "ingress_http",
            None,
            None,
            [
                ("cidr_ip", concrete_string("0.0.0.0/0")),
                ("ip_protocol", concrete_string("tcp")),
                ("description", concrete_string("Allow HTTP")),
            ],
        ),
        anonymous_state_entry(
            &schemas,
            "aws",
            "ec2.security_group_ingress",
            "ingress_https",
            None,
            None,
            [
                ("cidr_ip", concrete_string("0.0.0.0/0")),
                ("ip_protocol", concrete_string("tcp")),
                ("description", concrete_string("Allow HTTPS")),
            ],
        ),
    ];

    reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    // Names must remain unchanged - no swapping
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        "ingress_http",
        "ingress_http should not be renamed to ingress_https"
    );
    assert_eq!(
        resources[1].id.identity_str().expect("resolved identity"),
        "ingress_https",
        "ingress_https should not be renamed to ingress_http"
    );
}

fn make_sso_instance_registry() -> SchemaRegistry {
    let schema = PatternResourceSchema::new("sso.Instance").attribute(
        PatternAttributeSchema::optional("name", PatternAttributeType::string())
            .create_only()
            .build(),
    );
    let mut r = SchemaRegistry::new();
    r.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    r
}

fn lambda_permission_reference_registry() -> SchemaRegistry {
    let identity = TypeIdentity::new(Some("mock"), ["lambda", "Function"], "Name");
    let schema = PatternResourceSchema::new("lambda.Permission").attribute(
        PatternAttributeSchema::optional(
            "function_name",
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None),
        )
        .create_only()
        .build(),
    );
    let forms = ReferenceForms::from([(
        identity,
        ReferenceForm::Template {
            long: ReferenceTemplate::new(
                "arn:{partition}:lambda:{region}:{account}:function:{name}",
            )
            .expect("reference template must be valid"),
        },
    )]);
    let witness = ValidatedProviderSchemaWitness::new(
        vec![schema.validate().expect("schema fixture must be valid")],
        ValidatedConfigSchema::default(),
        forms,
    )
    .expect("provider schema witness must be valid");
    let mut registry = SchemaRegistry::new();
    registry
        .insert_provider_witness("mock", &witness)
        .expect("fixture provider witness must have one owner");
    registry
}

fn detect_reference_form_rename(
    resource: Resource,
    registry: &SchemaRegistry,
    state_entry: AnonymousIdStateInfo,
) -> Vec<(ResolvedResourceId, ResolvedResourceId)> {
    let resources = [resource];
    let claims = StateBlockClaims::empty();
    let candidates = resources
        .iter()
        .map(|resource| resource.id.clone())
        .collect::<HashSet<_>>();
    let claimed_origins = AnonymousToNamedClaimedOrigins::default();
    let providers = CanonicalizedProviderConfigs::from_configs_for_test(Vec::new());
    let scope = crate::provider::ReferenceScope::available("aws", "us-east-1", "123456789012")
        .expect("reference scope must be valid");
    let (_, comparison_provider) = crate::provider::comparison_fixture_with_scope_for_test(
        registry,
        resources.iter().map(|resource| &resource.id),
        scope,
    );
    let detection = super::detect_anonymous_to_named_renames(
        AnonymousToNamedCandidates::new(&resources, &candidates, &claimed_origins),
        registry,
        &comparison_provider,
        AnonymousToNamedStateLookup::new(
            &|provider, resource_type| {
                claims.screen_entries(
                    provider,
                    resource_type,
                    [state_entry.clone()],
                    AnonymousIdStateInfo::name,
                )
            },
            &|_binding| Vec::new(),
        ),
        &providers,
        &|_provider| Vec::new(),
        &claims,
    )
    .expect("reference-form comparison must resolve");
    let (moves, contests) = detection.into_parts();
    assert!(contests.is_empty());
    moves.into_iter().map(ResolvedMove::into_parts).collect()
}

#[test]
fn test_detect_rename_unique_match_by_create_only_attrs() {
    // Scenario: state has an anonymous sso.instance with name="carina-rs".
    // DSL now defines it as a let-bound resource with the same name.
    // detect_anonymous_to_named_renames should emit a rename from the
    // anonymous hash name to the binding name.
    let schemas = make_sso_instance_registry();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    resource.binding = Some(absolute_binding("sso"));
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "sso.Instance",
        "sso_instance_0ac0620303071530",
        None,
        None,
        [("name", concrete_string("carina-rs"))],
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].0.identity_str(), "sso_instance_0ac0620303071530");
    assert_eq!(renames[0].1.identity_str(), "sso");
}

#[test]
fn test_detect_rename_reference_form_matches_bare_name_and_arn() {
    let registry = lambda_permission_reference_registry();
    let mut resource = Resource::with_provider("mock", "lambda.Permission", "invoke", None);
    resource.binding = Some(absolute_binding("invoke"));
    resource.set_attr("function_name", concrete_string("processor"));
    let state_entry = anonymous_state_entry(
        &registry,
        "mock",
        "lambda.Permission",
        "mock_lambda_permission_01234567",
        None,
        None,
        [(
            "function_name",
            concrete_string("arn:aws:lambda:us-east-1:123456789012:function:processor"),
        )],
    );

    let renames = detect_reference_form_rename(resource, &registry, state_entry);

    assert_eq!(renames.len(), 1);
    assert_eq!(
        renames[0].0.identity_str(),
        "mock_lambda_permission_01234567"
    );
    assert_eq!(renames[0].1.identity_str(), "invoke");
}

#[test]
fn test_detect_rename_reference_form_prefix_uses_canonical_candidate_key() {
    let registry = lambda_permission_reference_registry();
    let mut resource = Resource::with_provider("mock", "lambda.Permission", "invoke", None);
    resource.binding = Some(absolute_binding("invoke"));
    resource
        .prefixes
        .insert("function_name".to_string(), "processor-".to_string());
    let state_entry = anonymous_state_entry(
        &registry,
        "mock",
        "lambda.Permission",
        "mock_lambda_permission_01234567",
        None,
        None,
        [(
            "function_name",
            concrete_string("arn:aws:lambda:us-east-1:123456789012:function:processor-prod"),
        )],
    );

    let renames = detect_reference_form_rename(resource, &registry, state_entry);

    assert_eq!(renames.len(), 1);
    assert_eq!(
        renames[0].0.identity_str(),
        "mock_lambda_permission_01234567"
    );
    assert_eq!(renames[0].1.identity_str(), "invoke");
}

#[test]
fn test_detect_rename_resolves_deferred_create_only_attrs_from_state_bindings() {
    let schemas = subnet_route_table_association_registry();

    let mut resource =
        subnet_route_table_association_resource("nat_gw", "private_rtb", "private_subnet");
    resource.binding = Some(absolute_binding("nat_gw"));
    let resources = vec![resource];
    let state_entries = vec![association_state_entry(
        &schemas,
        "awscc_ec2_subnet_route_table_association_11111111",
        "rtb-private",
        "subnet-private",
    )];
    let binding_entries = [
        binding_state_entry("private_rtb", "private_rtb", &[("id", "rtb-private")]),
        binding_state_entry(
            "private_subnet",
            "private_subnet",
            &[("id", "subnet-private")],
        ),
    ];

    let renames = detect_anonymous_to_named_renames_with_bindings_for_test(
        &resources,
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &|binding| binding_state_entries_for(&binding_entries, binding),
        &[],
        &|_provider| Vec::new(),
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(
        renames[0].0.identity_str(),
        "awscc_ec2_subnet_route_table_association_11111111"
    );
    assert_eq!(renames[0].1.identity_str(), "nat_gw");
}

#[test]
fn test_detect_rename_matches_map_create_only_attr() {
    let schema = PatternResourceSchema::new("test.MapIdentity").attribute(
        PatternAttributeSchema::optional(
            "settings",
            PatternAttributeType::map(PatternAttributeType::string()),
        )
        .create_only()
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let settings = Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "mode".to_string() => Value::Concrete(ConcreteValue::String("private".into())),
        "zone".to_string() => Value::Concrete(ConcreteValue::String("a".into())),
    }));
    let mut resource = Resource::with_provider("awscc", "test.MapIdentity", "mapped", None);
    resource.binding = Some(absolute_binding("mapped"));
    resource.set_attr("settings".to_string(), settings.clone());
    let resources = vec![resource];
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "test.MapIdentity",
        "awscc_test_map_identity_11111111",
        None,
        None,
        [("settings", settings)],
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].1.identity_str(), "mapped");
}

#[test]
fn test_detect_rename_matches_enum_and_int_create_only_attrs() {
    let schema = PatternResourceSchema::new("test.TypedIdentity")
        .attribute(
            PatternAttributeSchema::optional("domain", eip_domain_type("awscc"))
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("priority", PatternAttributeType::int())
                .create_only()
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let domain = Value::Concrete(ConcreteValue::CanonicalEnum(canonical_eip_domain(
        "awscc",
        "awscc.ec2.Eip.Domain.vpc",
    )));
    let priority = Value::Concrete(ConcreteValue::Int(7));
    let mut resource = Resource::with_provider("awscc", "test.TypedIdentity", "typed", None);
    resource.binding = Some(absolute_binding("typed"));
    resource.set_attr("domain".to_string(), domain.clone());
    resource.set_attr("priority".to_string(), priority.clone());
    let resources = vec![resource];
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "test.TypedIdentity",
        "awscc_test_typed_identity_11111111",
        None,
        None,
        [
            ("domain", concrete_string("vpc")),
            ("priority", Value::Concrete(ConcreteValue::Int(7))),
        ],
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].1.identity_str(), "typed");
}

#[test]
fn test_detect_rename_matches_mixed_canonical_and_identifier_enums_in_list_and_map() {
    let domain = eip_domain_type("awscc");
    let validated_schema = PatternResourceSchema::new("ec2.Eip")
        .attribute(
            PatternAttributeSchema::optional("domains", PatternAttributeType::list(domain.clone()))
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("domain_by_name", PatternAttributeType::map(domain))
                .create_only()
                .build(),
        )
        .validate()
        .expect("nested enum schema fixture must be valid");
    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", validated_schema.clone());

    let canonical = Value::Concrete(ConcreteValue::CanonicalEnum(canonical_eip_domain(
        "aws",
        "aws.ec2.Eip.Domain.vpc",
    )));
    let identifier = Value::Concrete(ConcreteValue::enum_identifier(
        "awscc.ec2.Eip.Domain.standard".to_string(),
    ));
    let mut resource = Resource::with_provider("awscc", "ec2.Eip", "typed", None);
    resource.binding = Some(absolute_binding("typed"));
    resource.set_attr(
        "domains".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            canonical.clone(),
            identifier.clone(),
        ])),
    );
    resource.set_attr(
        "domain_by_name".to_string(),
        Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "canonical".to_string() => canonical,
            "identifier".to_string() => identifier,
        })),
    );

    let persisted_attributes = HashMap::from([
        (
            "domains".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("vpc".into())),
                Value::Concrete(ConcreteValue::String("standard".into())),
            ])),
        ),
        (
            "domain_by_name".to_string(),
            Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                "canonical".to_string() => Value::Concrete(ConcreteValue::String("vpc".into())),
                "identifier".to_string() => Value::Concrete(ConcreteValue::String("standard".into())),
            })),
        ),
    ]);
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.Eip",
        "awscc_ec2_eip_11111111",
        None,
        None,
        persisted_attributes,
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &[resource],
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].0.identity_str(), "awscc_ec2_eip_11111111");
    assert_eq!(renames[0].1.identity_str(), "typed");
}

#[test]
fn test_detect_rename_skips_ambiguous_resolved_create_only_matches() {
    let schemas = subnet_route_table_association_registry();
    let mut resource =
        subnet_route_table_association_resource("nat_gw", "private_rtb", "private_subnet");
    resource.binding = Some(absolute_binding("nat_gw"));
    let resources = vec![resource];
    let state_entries = vec![
        association_state_entry(
            &schemas,
            "awscc_ec2_subnet_route_table_association_11111111",
            "rtb-private",
            "subnet-private",
        ),
        association_state_entry(
            &schemas,
            "awscc_ec2_subnet_route_table_association_22222222",
            "rtb-private",
            "subnet-private",
        ),
    ];
    let binding_entries = [
        binding_state_entry("private_rtb", "private_rtb", &[("id", "rtb-private")]),
        binding_state_entry(
            "private_subnet",
            "private_subnet",
            &[("id", "subnet-private")],
        ),
    ];

    let renames = detect_anonymous_to_named_renames_with_bindings_for_test(
        &resources,
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &|binding| binding_state_entries_for(&binding_entries, binding),
        &[],
        &|_provider| Vec::new(),
    );

    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_falls_back_to_simhash_when_state_lacks_create_only_attr() {
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_provider: &str| Vec::new();
    let schemas = subnet_route_table_association_registry();
    let mut legacy_schemas = SchemaRegistry::new();
    legacy_schemas.insert(
        "awscc",
        PatternResourceSchema::new("ec2.SubnetRouteTableAssociation")
            .attribute(
                PatternAttributeSchema::optional("route_table_id", PatternAttributeType::string())
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional("subnet_id", PatternAttributeType::string())
                    .build(),
            )
            .validate()
            .expect("legacy schema fixture must be valid"),
    );

    let mut anonymous =
        subnet_route_table_association_resource("placeholder", "private_rtb", "private_subnet");
    anonymous.id =
        ResourceId::pending_with_provider("awscc", "ec2.SubnetRouteTableAssociation", None);
    let mut anonymous_resources = vec![anonymous];
    compute_anonymous_identifiers_for_test(
        &mut anonymous_resources,
        &providers,
        &legacy_schemas,
        &identity_fn,
    )
    .expect("legacy anonymous SimHash must compute");
    let anonymous_name = anonymous_resources[0]
        .id
        .identity_str()
        .expect("legacy anonymous identity must resolve")
        .to_string();
    assert!(matches!(
        extract_hash_from_identifier(&anonymous_name),
        Some(AnonymousHashSuffix::SimHash(_))
    ));

    let mut named = anonymous_resources.remove(0);
    named.id = ResourceId::with_provider_identity(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        "nat_gw",
        None,
    );
    named.binding = Some(absolute_binding("nat_gw"));
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        &anonymous_name,
        None,
        None,
        [("route_table_id", concrete_string("rtb-private"))],
    )];
    let binding_entries = [
        binding_state_entry("private_rtb", "private_rtb", &[("id", "rtb-private")]),
        binding_state_entry(
            "private_subnet",
            "private_subnet",
            &[("id", "subnet-private")],
        ),
    ];

    let renames = detect_anonymous_to_named_renames_with_bindings_for_test(
        &[named],
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &|binding| binding_state_entries_for(&binding_entries, binding),
        &providers,
        &identity_fn,
    );

    assert_eq!(
        renames.len(),
        1,
        "zero create-only matches must use SimHash"
    );
    assert_eq!(renames[0].0.identity_str(), anonymous_name);
    assert_eq!(renames[0].1.identity_str(), "nat_gw");

    let ambiguous_state_entries = vec![
        association_state_entry(&schemas, &anonymous_name, "rtb-private", "subnet-private"),
        association_state_entry(
            &schemas,
            "awscc_ec2_subnet_route_table_association_0000000000000000",
            "rtb-private",
            "subnet-private",
        ),
    ];
    let mut ambiguous_resource =
        subnet_route_table_association_resource("nat_gw", "private_rtb", "private_subnet");
    ambiguous_resource.binding = Some(absolute_binding("nat_gw"));
    let ambiguous = detect_anonymous_to_named_renames_with_bindings_for_test(
        &[ambiguous_resource],
        &schemas,
        &|_provider, _resource_type| ambiguous_state_entries.clone(),
        &|binding| binding_state_entries_for(&binding_entries, binding),
        &providers,
        &identity_fn,
    );
    assert!(
        ambiguous.is_empty(),
        "ambiguous create-only matches must never fall back to the exact SimHash row",
    );
}

#[test]
fn test_detect_rename_simhash_fallback_rejects_explicit_create_only_mismatches() {
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_provider: &str| Vec::new();
    let mut legacy_schemas = SchemaRegistry::new();
    legacy_schemas.insert(
        "awscc",
        PatternResourceSchema::new("ec2.Eip")
            .attribute(
                PatternAttributeSchema::optional("allocation_id", PatternAttributeType::string())
                    .build(),
            )
            .validate()
            .expect("legacy schema fixture must be valid"),
    );

    let mut anonymous = Resource::pending_with_provider("awscc", "ec2.Eip", None);
    anonymous.set_attr(
        "allocation_id".to_string(),
        Value::Concrete(ConcreteValue::String("eip-c".into())),
    );
    let mut anonymous_resources = vec![anonymous];
    compute_anonymous_identifiers_for_test(
        &mut anonymous_resources,
        &providers,
        &legacy_schemas,
        &identity_fn,
    )
    .expect("legacy anonymous SimHash must compute");
    let exact_simhash_name = anonymous_resources[0]
        .id
        .identity_str()
        .expect("legacy anonymous identity must resolve")
        .to_string();
    assert!(matches!(
        extract_hash_from_identifier(&exact_simhash_name),
        Some(AnonymousHashSuffix::SimHash(_))
    ));

    let mut nearby_chars = exact_simhash_name.chars().collect::<Vec<_>>();
    let last = nearby_chars.last_mut().expect("SimHash name is non-empty");
    *last = if *last == '0' { '1' } else { '0' };
    let nearby_simhash_name = nearby_chars.into_iter().collect::<String>();

    let mut named = anonymous_resources.remove(0);
    named.id = ResourceId::with_provider_identity("awscc", "ec2.Eip", "eip", None);
    named.binding = Some(absolute_binding("eip"));

    let mut current_schemas = SchemaRegistry::new();
    current_schemas.insert(
        "awscc",
        PatternResourceSchema::new("ec2.Eip")
            .attribute(
                PatternAttributeSchema::optional("allocation_id", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("current schema fixture must be valid"),
    );
    let state_entries = vec![
        anonymous_state_entry(
            &current_schemas,
            "awscc",
            "ec2.Eip",
            &exact_simhash_name,
            None,
            None,
            [("allocation_id", concrete_string("eip-a"))],
        ),
        anonymous_state_entry(
            &current_schemas,
            "awscc",
            "ec2.Eip",
            &nearby_simhash_name,
            None,
            None,
            [("allocation_id", concrete_string("eip-b"))],
        ),
    ];

    let renames = detect_anonymous_to_named_renames_for_test(
        &[named],
        &current_schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &providers,
        &identity_fn,
    );

    assert!(
        renames.is_empty(),
        "SimHash fallback must not override explicit create-only mismatches: {renames:?}",
    );
}

#[test]
fn test_detect_rename_does_not_match_unresolvable_create_only_ref() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        route_schema_with_create_only_route_table()
            .validate()
            .expect("schema fixture must be valid"),
    );
    let mut resource = route_with_deferred_route_table("route", "missing_rtb");
    resource.binding = Some(absolute_binding("route"));
    let resources = vec![resource];
    let state_entries = vec![route_state_entry(
        &schemas,
        "awscc_ec2_route_11111111",
        "rtb-private",
    )];

    let renames = detect_anonymous_to_named_renames_with_bindings_for_test(
        &resources,
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &|_binding| Vec::new(),
        &[],
        &|_provider| Vec::new(),
    );

    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_invalid_enum_is_typed_unresolved_and_does_not_rename() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        PatternResourceSchema::new("ec2.Eip")
            .attribute(
                PatternAttributeSchema::optional("domain", eip_domain_type("awscc"))
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("enum schema fixture must be valid"),
    );
    let mut resource = Resource::with_provider("awscc", "ec2.Eip", "eip", None);
    resource.binding = Some(absolute_binding("eip"));
    resource.set_attr(
        "domain".to_string(),
        Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.ec2.Eip.Domain.invalid".to_string(),
        )),
    );

    let schema = schemas
        .get_for(&resource)
        .expect("resource schema exists")
        .schema();
    let resolution = canonical_or_resolved_create_only_value(
        resource
            .get_attr("domain")
            .expect("domain is configured on the test resource"),
        schema.as_schema(),
        "domain",
        &|_binding| Vec::new(),
    );
    let Err(UnresolvedReason::Type(error)) = resolution else {
        panic!("invalid enum must return its typed canonicalization error")
    };
    assert!(matches!(
        error,
        crate::schema::TypeError::InvalidEnumVariant { .. }
    ));

    let create_only_values =
        DesiredCreateOnlyValues::without_binding_resolution(&resource, schema.as_schema());
    assert!(matches!(
        create_only_values.values.get("domain"),
        Some(DesiredCreateOnlyValue::Unresolved)
    ));

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "ec2.Eip",
        "awscc_ec2_eip_11111111",
        None,
        None,
        [("domain", concrete_string("vpc"))],
    )];
    let renames = detect_anonymous_to_named_renames_for_test(
        &[resource],
        &schemas,
        &|_provider, _resource_type| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );

    assert!(renames.is_empty());
}

#[test]
fn test_detect_anonymous_to_named_skips_claimed_from() {
    let schemas = make_sso_instance_registry();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    resource.binding = Some(absolute_binding("sso"));
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];
    let state_name = "sso_instance_0ac0620303071530";
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "sso.Instance",
        state_name,
        None,
        None,
        [("name", concrete_string("carina-rs"))],
    )];
    let claims = StateBlockClaims::new(
        [StateBlockAddress::new("awscc", "sso.Instance", state_name)]
            .into_iter()
            .collect(),
        HashSet::new(),
    );

    let renames = super::detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                state_entries.clone(),
                AnonymousIdStateInfo::name,
            )
        },
        &|_binding| Vec::new(),
        &[],
        &|_provider| Vec::new(),
        &claims,
    );

    assert!(
        renames.is_empty(),
        "anonymous-to-named detector must not synthesize a rename from a claimed from"
    );
}

#[test]
fn test_detect_anonymous_to_named_skips_claimed_to() {
    let schemas = make_sso_instance_registry();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    resource.binding = Some(absolute_binding("sso"));
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];
    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "sso.Instance",
        "sso_instance_0ac0620303071530",
        None,
        None,
        [("name", concrete_string("carina-rs"))],
    )];
    let claims = StateBlockClaims::new(
        HashSet::new(),
        [StateBlockAddress::new("awscc", "sso.Instance", "sso")]
            .into_iter()
            .collect(),
    );

    let renames = super::detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                state_entries.clone(),
                AnonymousIdStateInfo::name,
            )
        },
        &|_binding| Vec::new(),
        &[],
        &|_provider| Vec::new(),
        &claims,
    );

    assert!(
        renames.is_empty(),
        "anonymous-to-named detector must not synthesize a rename onto a claimed to"
    );
}

#[test]
fn test_reconcile_skips_state_entry_claimed_by_removed_from() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (route_schema_with_create_only_route_table())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let resources = vec![route_with_deferred_route_table(
        "awscc_ec2_route_aaaaaaaa",
        "private_rtb",
    )];
    let state_name = "awscc_ec2_route_11111111";
    let state_entries = vec![route_state_entry(&schemas, state_name, "rtb-private")];
    let binding_entries = [binding_state_entry(
        "private_rtb",
        "private_rtb",
        &[("id", "rtb-private")],
    )];
    let claims = StateBlockClaims::new(
        [StateBlockAddress::new("awscc", "ec2.Route", state_name)]
            .into_iter()
            .collect(),
        HashSet::new(),
    );
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        &schemas,
        resources.iter().map(|resource| &resource.id),
    );

    super::reconcile_anonymous_identifiers(
        &resources,
        &[],
        &schemas,
        &comparison_provider,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                state_entries.clone(),
                AnonymousIdStateInfo::name,
            )
        },
        &|binding| {
            binding_entries
                .iter()
                .filter(|(entry_binding, _)| entry_binding == binding)
                .map(|(_, entry)| entry.clone())
                .collect()
        },
        &claims,
    )
    .expect("create-only comparison fixture must resolve");

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        "awscc_ec2_route_aaaaaaaa",
        "effective removed.from state row must be excluded from heuristic matching"
    );
}

#[test]
fn test_reconcile_existing_removed_from_identity_does_not_repoint_to_orphan() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (subnet_route_table_association_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let desired_name = "awscc_ec2_subnet_route_table_association_aaaaaaaa";
    let orphan_name = "awscc_ec2_subnet_route_table_association_bbbbbbbb";
    let mut resource = Resource::with_provider(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        desired_name,
        None,
    );
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Concrete(ConcreteValue::String("rtb-private".into())),
    );
    resource.set_attr(
        "subnet_id".to_string(),
        Value::Concrete(ConcreteValue::String("subnet-current".into())),
    );
    let resources = vec![resource];
    let state_entries = vec![
        association_state_entry(&schemas, desired_name, "rtb-private", "subnet-current"),
        association_state_entry(&schemas, orphan_name, "rtb-private", "subnet-orphan"),
    ];
    let claims = StateBlockClaims::new(
        [StateBlockAddress::new(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            desired_name,
        )]
        .into_iter()
        .collect(),
        HashSet::new(),
    );
    let (_, comparison_provider) = crate::provider::comparison_fixture_for_test(
        &schemas,
        resources.iter().map(|resource| &resource.id),
    );

    let renames = super::reconcile_anonymous_identifiers(
        &resources,
        &[],
        &schemas,
        &comparison_provider,
        &|provider, resource_type| {
            claims.screen_entries(provider, resource_type, state_entries.clone(), |entry| {
                entry.name()
            })
        },
        &|_| Vec::new(),
        &claims,
    )
    .expect("create-only comparison fixture must resolve");

    assert!(renames.is_empty());
    assert_eq!(
        resources[0].id.identity_str(),
        Some(desired_name),
        "an existing removed.from-claimed identity must remain visible to the existence check",
    );
}

#[test]
fn test_detect_rename_skips_when_binding_already_in_state() {
    // If state already has an entry for the binding name, nothing to rename.
    let schemas = make_sso_instance_registry();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    resource.binding = Some(absolute_binding("sso"));
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "sso.Instance",
        "sso",
        None,
        None,
        [("name", concrete_string("carina-rs"))],
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_existing_removed_from_binding_does_not_use_orphan() {
    let schemas = make_sso_instance_registry();
    let binding_name = "sso";
    let orphan_name = "sso_instance_0ac0620303071530";
    let mut resource = Resource::with_provider("awscc", "sso.Instance", binding_name, None);
    resource.binding = Some(absolute_binding(binding_name));
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];
    let state_entries = vec![
        anonymous_state_entry(
            &schemas,
            "awscc",
            "sso.Instance",
            binding_name,
            None,
            None,
            [("name", concrete_string("carina-rs"))],
        ),
        anonymous_state_entry(
            &schemas,
            "awscc",
            "sso.Instance",
            orphan_name,
            None,
            None,
            [("name", concrete_string("carina-rs"))],
        ),
    ];
    let claims = StateBlockClaims::new(
        [StateBlockAddress::new(
            "awscc",
            "sso.Instance",
            binding_name,
        )]
        .into_iter()
        .collect(),
        HashSet::new(),
    );

    let renames = super::detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|provider, resource_type| {
            claims.screen_entries(
                provider,
                resource_type,
                state_entries.clone(),
                AnonymousIdStateInfo::name,
            )
        },
        &|_binding| Vec::new(),
        &[],
        &|_| Vec::new(),
        &claims,
    );

    assert!(
        renames.is_empty(),
        "an existing removed.from-claimed binding must suppress orphan rename detection",
    );
    assert_eq!(resources[0].id.identity_str(), Some(binding_name));
}

#[test]
fn test_detect_rename_ignores_anonymous_resources() {
    // Anonymous resources (binding=None) are not candidates for this rename.
    let schemas = make_sso_instance_registry();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso_instance_new", None);
    // No binding set
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "sso.Instance",
        "sso_instance_0ac0620303071530",
        None,
        None,
        [("name", concrete_string("carina-rs"))],
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_skips_ambiguous_matches() {
    // Two orphan state entries match — skip to avoid rebinding the wrong one.
    let schemas = make_sso_instance_registry();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    resource.binding = Some(absolute_binding("sso"));
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];

    let state_entries = vec![
        anonymous_state_entry(
            &schemas,
            "awscc",
            "sso.Instance",
            "sso_instance_aaaabbbbccccdddd",
            None,
            None,
            [("name", concrete_string("carina-rs"))],
        ),
        anonymous_state_entry(
            &schemas,
            "awscc",
            "sso.Instance",
            "sso_instance_1111222233334444",
            None,
            None,
            [("name", concrete_string("carina-rs"))],
        ),
    ];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

#[test]
fn test_detect_rename_ignores_non_hash_state_names() {
    // A state entry with a non-hash name (e.g., another let binding) is not
    // treated as an anonymous candidate and must not be silently renamed.
    let schemas = make_sso_instance_registry();

    let mut resource = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    resource.binding = Some(absolute_binding("sso"));
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![resource];

    let state_entries = vec![anonymous_state_entry(
        &schemas,
        "awscc",
        "sso.Instance",
        "my_custom_binding",
        None,
        None,
        [("name", concrete_string("carina-rs"))],
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &[],
        &|_provider| Vec::new(),
    );
    assert!(renames.is_empty());
}

/// Registry with NO create-only attributes — like `awscc.sso.Instance`.
fn make_sso_instance_registry_no_create_only() -> SchemaRegistry {
    let schema = PatternResourceSchema::new("sso.Instance").attribute(
        PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
    );
    let mut r = SchemaRegistry::new();
    r.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    r
}

#[test]
fn test_detect_rename_no_create_only_matches_by_simhash() {
    // Regression test for carina#1670:
    // Schema has no create-only attrs (e.g. awscc.sso.Instance). The
    // anonymous → let-bound rename must still be detected so `carina plan`
    // shows a Move rather than Delete+Create, which would destroy the
    // Identity Center instance and all of its users/groups.
    let schemas = make_sso_instance_registry_no_create_only();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Step 1: generate the anonymous ID the previous `apply` would have
    // written to state, using the same inputs and the same code path.
    let mut anon = Resource::pending_with_provider("awscc", "sso.Instance", None);
    anon.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers_for_test(&mut anon_vec, &providers, &schemas, &identity_fn)
        .unwrap();
    let anonymous_name = anon_vec[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    assert!(
        anonymous_name.starts_with("awscc_sso_instance_"),
        "expected hash-derived name, got {anonymous_name}"
    );

    // Step 2: user wraps the same resource in a `let` binding.
    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    let_bound.binding = Some(absolute_binding("sso"));
    let_bound.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![let_bound];

    // Step 3: state still has the orphan anonymous entry.
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas,
        "awscc",
        "sso.Instance",
        &anonymous_name,
    )];

    // Step 4: detect_anonymous_to_named_renames should match via SimHash.
    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );

    assert_eq!(renames.len(), 1, "expected one rename, got {:?}", renames);
    assert_eq!(renames[0].0.identity_str(), anonymous_name);
    assert_eq!(renames[0].1.identity_str(), "sso");
}

#[test]
fn test_detect_rename_no_create_only_skips_when_attributes_differ_too_much() {
    // If the let-bound resource's attributes drift beyond the SimHash
    // Hamming threshold from any orphan, no rename is emitted and the
    // user falls back to delete+create (or a `moved` block if they want
    // to preserve state).
    let schemas = make_sso_instance_registry_no_create_only();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Anonymous snapshot with many attributes.
    let mut anon = Resource::pending_with_provider("awscc", "sso.Instance", None);
    anon.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("old-name".into())),
    );
    let mut tags = indexmap::IndexMap::new();
    tags.insert(
        "k1".to_string(),
        Value::Concrete(ConcreteValue::String("v1".into())),
    );
    tags.insert(
        "k2".to_string(),
        Value::Concrete(ConcreteValue::String("v2".into())),
    );
    anon.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags)),
    );
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers_for_test(&mut anon_vec, &providers, &schemas, &identity_fn)
        .unwrap();
    let anonymous_name = anon_vec[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Let-bound resource with wildly different attributes.
    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    let_bound.binding = Some(absolute_binding("sso"));
    let_bound.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("completely-different".into())),
    );
    let mut different_tags = indexmap::IndexMap::new();
    different_tags.insert(
        "other1".to_string(),
        Value::Concrete(ConcreteValue::String("foo".into())),
    );
    different_tags.insert(
        "other2".to_string(),
        Value::Concrete(ConcreteValue::String("bar".into())),
    );
    different_tags.insert(
        "other3".to_string(),
        Value::Concrete(ConcreteValue::String("baz".into())),
    );
    let_bound.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(different_tags)),
    );
    let resources = vec![let_bound];

    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas,
        "awscc",
        "sso.Instance",
        &anonymous_name,
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );
    assert!(
        renames.is_empty(),
        "attributes differ too much, should not rename: {:?}",
        renames
    );
}

#[test]
fn test_detect_rename_no_create_only_picks_closest_among_multiple_candidates() {
    // Two orphans: one is an exact SimHash match, the other is off by a
    // few bits but still within the Hamming threshold. The exact match
    // must win — this exercises the `distance < d` branch of the picker.
    let schemas = make_sso_instance_registry_no_create_only();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Compute the exact-match name.
    let mut anon = Resource::pending_with_provider("awscc", "sso.Instance", None);
    anon.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers_for_test(&mut anon_vec, &providers, &schemas, &identity_fn)
        .unwrap();
    let exact_name = anon_vec[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Construct a "close but not equal" orphan by flipping the last hex
    // char of the SimHash — guarantees a small nonzero Hamming distance
    // well under the threshold.
    let mut chars: Vec<char> = exact_name.chars().collect();
    let last = chars.last_mut().unwrap();
    *last = if *last == '0' { 'f' } else { '0' };
    let nearby_name: String = chars.into_iter().collect();
    assert_ne!(exact_name, nearby_name);

    // Place the nearby entry first so the picker must prefer the later
    // exact match via the `distance < d` branch.
    let state_entries = vec![
        anonymous_state_entry_without_attributes(&schemas, "awscc", "sso.Instance", &nearby_name),
        anonymous_state_entry_without_attributes(&schemas, "awscc", "sso.Instance", &exact_name),
    ];

    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    let_bound.binding = Some(absolute_binding("sso"));
    let_bound.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![let_bound];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );

    assert_eq!(renames.len(), 1);
    assert_eq!(
        renames[0].0.identity_str(),
        exact_name,
        "should prefer the exact SimHash match over the nearby one"
    );
}

#[test]
fn test_detect_rename_no_create_only_skips_8_char_hash_entries() {
    // An 8-hex state entry (from the create-only hash path) must not
    // match against a 16-hex SimHash — the hashes use different schemes
    // and comparing them by Hamming distance is meaningless.
    let schemas = make_sso_instance_registry_no_create_only();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    let_bound.binding = Some(absolute_binding("sso"));
    let_bound.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![let_bound];

    // 8-hex suffix (standard hash scheme), not a SimHash.
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas,
        "awscc",
        "sso.Instance",
        "sso_instance_a3f2b1c8",
    )];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );
    assert!(
        renames.is_empty(),
        "8-hex entries must not match the SimHash branch: {:?}",
        renames
    );
}

#[test]
fn test_detect_rename_no_create_only_skips_when_two_orphans_tie_on_distance() {
    // Two orphans both within the Hamming threshold with identical distance
    // to the let-bound resource — ambiguous, must skip.
    let schemas = make_sso_instance_registry_no_create_only();
    let providers: Vec<ProviderConfig> = Vec::new();
    let identity_fn = |_: &str| -> Vec<String> { Vec::new() };

    // Compute the target SimHash.
    let mut anon = Resource::pending_with_provider("awscc", "sso.Instance", None);
    anon.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let mut anon_vec = vec![anon];
    compute_anonymous_identifiers_for_test(&mut anon_vec, &providers, &schemas, &identity_fn)
        .unwrap();
    let anonymous_name = anon_vec[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    // Two state entries with the exact same name hash → same distance (0).
    let state_entries = vec![
        anonymous_state_entry_without_attributes(
            &schemas,
            "awscc",
            "sso.Instance",
            &anonymous_name,
        ),
        anonymous_state_entry_without_attributes(
            &schemas,
            "awscc",
            "sso.Instance",
            &anonymous_name,
        ),
    ];

    let mut let_bound = Resource::with_provider("awscc", "sso.Instance", "sso", None);
    let_bound.binding = Some(absolute_binding("sso"));
    let_bound.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("carina-rs".into())),
    );
    let resources = vec![let_bound];

    let renames = detect_anonymous_to_named_renames_for_test(
        &resources,
        &schemas,
        &|_provider, _rt| state_entries.clone(),
        &providers,
        &identity_fn,
    );
    assert!(
        renames.is_empty(),
        "two orphans tie on distance, should skip to avoid rebinding wrong entry: {:?}",
        renames
    );
}

#[test]
fn anonymous_identifier_includes_provider_prefix() {
    let schema = PatternResourceSchema::new("iam.RolePolicy").attribute(
        PatternAttributeSchema::optional("policy_name", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut r = Resource::pending_with_provider("awscc", "iam.RolePolicy", None);
    r.set_attr(
        "policy_name".to_string(),
        Value::Concrete(ConcreteValue::String("foo".into())),
    );
    let mut resources = vec![r];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();

    let name = resources[0].id.identity_str().expect("resolved identity");
    assert!(
        name.starts_with("awscc_"),
        "identifier should begin with the provider prefix, got: {name}"
    );
    assert!(
        name.contains("iam_role_policy_"),
        "identifier should still contain the snake-case type, got: {name}"
    );
}

#[test]
fn anonymous_identifier_provider_prefix_for_aws_provider() {
    let schema = PatternResourceSchema::new("s3.Bucket").attribute(
        PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "aws".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    let mut r = Resource::pending_with_provider("aws", "s3.Bucket", None);
    r.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("example".into())),
    );
    let mut resources = vec![r];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();
    assert!(
        resources[0]
            .id
            .identity_str()
            .unwrap_or("")
            .starts_with("aws_s3_bucket_"),
        "identifier should begin with `aws_s3_bucket_`, got: {}",
        resources[0].id.identity_str().expect("resolved identity")
    );
}

#[test]
fn reconcile_simhash_match_keeps_new_format_identifier_and_emits_move() {
    // Schema with NO create-only attrs forces the SimHash-distance branch.
    let schema = PatternResourceSchema::new("iam.RolePolicy").attribute(
        PatternAttributeSchema::optional("policy_name", PatternAttributeType::string()).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let providers = vec![ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let identity_fn = |_: &str| -> Vec<String> { vec![] };

    // Build a resource and let compute_anonymous_identifiers assign its
    // freshly-computed new-format name.
    let mut r = Resource::pending_with_provider("awscc", "iam.RolePolicy", None);
    r.set_attr(
        "policy_name".to_string(),
        Value::Concrete(ConcreteValue::String("inline".into())),
    );
    let mut resources = vec![r];
    compute_anonymous_identifiers_for_test(&mut resources, &providers, &schemas, &identity_fn)
        .unwrap();
    let new_name = resources[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();
    assert!(new_name.starts_with("awscc_iam_role_policy_"));

    // Strip the provider prefix to simulate a state entry written under the
    // older identifier format.
    let old_name = new_name
        .strip_prefix("awscc_")
        .expect("new format starts with awscc_ prefix")
        .to_string();
    let state_entries = vec![anonymous_state_entry_without_attributes(
        &schemas,
        "awscc",
        "iam.RolePolicy",
        &old_name,
    )];

    let renames = reconcile_anonymous_identifiers(
        &resources,
        &schemas,
        &|_p, _rt| state_entries.clone(),
        &|_binding| Vec::new(),
    );

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        new_name,
        "resource id should retain the new-format identifier",
    );
    assert_eq!(
        resolved_move_identity_pairs(&renames),
        vec![(old_name, new_name)],
        "should emit a move from the old state identity to the new identifier",
    );
}

#[test]
fn loop_attribute_derived_conflict_renders_exact_rewrite() {
    let source = "for cidr in ['10.0.0.0/16', '10.1.0.0/16'] {\n  mock.ec2.Vpc {\n    identity_input = cidr\n  }\n}";
    let mut resources = parse_loop_fixture(source, "loops.crn");
    let registry = loop_identity_registry(false);

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("one unbound loop cannot safely emit mutable-derived peers");

    assert!(matches!(error, AnonymousIdentityError::Loop(_)));
    assert_eq!(
        error.to_string(),
        concat!(
            "Anonymous resources produced by the unbound `for` at loops.crn:1 cannot be ",
            "identified safely because multiple 'mock.ec2.Vpc' elements derive identity ",
            "from mutable attributes.\n",
            "Rewrite the loop with a binding and migrate its existing state addresses:\n\n",
            "let vpc = for cidr in ['10.0.0.0/16', '10.1.0.0/16'] {\n",
            "  mock.ec2.Vpc {\n",
            "    identity_input = cidr\n",
            "  }\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.ec2.Vpc '_for0[0]'\n",
            "  to = mock.ec2.Vpc 'vpc[0]'\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.ec2.Vpc '_for0[1]'\n",
            "  to = mock.ec2.Vpc 'vpc[1]'\n",
            "}",
        )
    );
}

#[test]
fn attribute_derived_plain_and_single_loop_member_report_both_typed_errors() {
    let mut resources = vec![
        Resource::pending_with_provider("mock", "ec2.Vpc", None).with_attribute(
            "identity_input",
            Value::Concrete(ConcreteValue::String("plain".into())),
        ),
    ];
    resources.extend(parse_loop_fixture(
        "for cidr in ['10.0.0.0/16'] {\n  mock.ec2.Vpc { identity_input = cidr }\n}",
        "loop.crn",
    ));

    let error = compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &loop_identity_registry(false),
        &|_| Vec::new(),
    )
    .expect_err("a plain declaration and one loop element are ambiguous peers");
    let rendered = error.to_string();

    assert!(
        rendered.contains("Anonymous resource identity is derived from mutable attributes"),
        "plain member lost its typed error: {rendered}",
    );
    assert!(
        rendered.contains("unbound `for` at loop.crn:1"),
        "single loop member lost its rewrite error: {rendered}",
    );
    assert!(rendered.contains("from = mock.ec2.Vpc '_for0[0]'"));
}

#[test]
fn attribute_derived_single_element_loops_in_sibling_files_report_each_loop() {
    let mut resources = parse_loop_fixture(
        "for cidr in ['10.0.0.0/16'] {\n  mock.ec2.Vpc { identity_input = cidr }\n}",
        "a.crn",
    );
    resources.extend(parse_loop_fixture(
        "for cidr in ['10.1.0.0/16'] {\n  mock.ec2.Vpc { identity_input = cidr }\n}",
        "b.crn",
    ));

    let error = compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &loop_identity_registry(false),
        &|_| Vec::new(),
    )
    .expect_err("both sibling loops require independent rewrites");
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("Anonymous resources produced by").count(),
        2
    );
    assert!(rendered.contains("unbound `for` at a.crn:1"));
    assert!(rendered.contains("unbound `for` at b.crn:1"));
}

#[test]
fn attribute_derived_distinct_single_element_loops_report_two_rewrites() {
    let mut resources = parse_loop_fixture(
        "for cidr in ['10.0.0.0/16'] {\n  mock.ec2.Vpc { identity_input = cidr }\n}\n\
         for cidr in ['10.1.0.0/16'] {\n  mock.ec2.Vpc { identity_input = cidr }\n}",
        "loops.crn",
    );

    let error = compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &loop_identity_registry(false),
        &|_| Vec::new(),
    )
    .expect_err("each authored loop source requires its own rewrite");
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("Anonymous resources produced by").count(),
        2
    );
    assert!(rendered.contains("unbound `for` at loops.crn:1"));
    assert!(rendered.contains("unbound `for` at loops.crn:4"));
    assert!(rendered.contains("let vpc = for cidr"));
    assert!(rendered.contains("let vpc_2 = for cidr"));
}

#[test]
fn loop_identical_identity_inputs_renders_exact_moves() {
    let source = "let entries = {\n  alpha = 'same'\n  \"can't\" = 'same'\n}\nfor key, name in entries {\n  mock.ec2.Vpc {\n    identity_input = name\n  }\n}";
    let mut resources = parse_loop_fixture(source, "quoted-key.crn");
    let registry = loop_identity_registry(true);

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("identical stable inputs must not silently share an identity");

    assert!(matches!(error, AnonymousIdentityError::Loop(_)));
    assert_eq!(
        error.to_string(),
        concat!(
            "Anonymous resources produced by the unbound `for` at quoted-key.crn:5 cannot be ",
            "identified safely because multiple 'mock.ec2.Vpc' elements have identical stable ",
            "identity inputs.\n",
            "Rewrite the loop with a binding and migrate its existing state addresses:\n\n",
            "let vpc = for key, name in entries {\n",
            "  mock.ec2.Vpc {\n",
            "    identity_input = name\n",
            "  }\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.ec2.Vpc '_entries.alpha'\n",
            "  to = mock.ec2.Vpc 'vpc.alpha'\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.ec2.Vpc \"_entries['can't']\"\n",
            "  to = mock.ec2.Vpc \"vpc['can't']\"\n",
            "}",
        )
    );
}

#[test]
fn loop_identical_identity_inputs_rewrite_covers_noncolliding_elements() {
    let source =
        "for name in ['same', 'same', 'other'] {\n  mock.ec2.Vpc { identity_input = name }\n}";
    let mut resources = parse_loop_fixture(source, "partial-collision.crn");

    let error = compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &loop_identity_registry(true),
        &|_| Vec::new(),
    )
    .expect_err("one colliding pair must require one complete loop rewrite");
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("Anonymous resources produced by").count(),
        1,
        "one authored loop must produce one error: {rendered}",
    );
    assert_eq!(
        rendered.matches("moved {").count(),
        3,
        "the rewrite must migrate every loop element: {rendered}",
    );
    assert!(rendered.contains("from = mock.ec2.Vpc '_for0[2]'"));
    assert!(rendered.contains("to = mock.ec2.Vpc 'vpc[2]'"));
}

#[test]
fn loop_multiple_identity_collision_buckets_emit_one_complete_rewrite() {
    let source = "for name in ['a', 'a', 'b', 'b'] {\n  mock.ec2.Vpc { identity_input = name }\n}";
    let mut resources = parse_loop_fixture(source, "multiple-collisions.crn");

    let error = compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &loop_identity_registry(true),
        &|_| Vec::new(),
    )
    .expect_err("all collision buckets belong to one authored loop");
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("Anonymous resources produced by").count(),
        1,
        "one authored loop must produce one error: {rendered}",
    );
    assert_eq!(
        rendered.matches("moved {").count(),
        4,
        "the rewrite must migrate all four elements: {rendered}",
    );
    assert!(rendered.contains("from = mock.ec2.Vpc '_for0[3]'"));
    assert!(rendered.contains("to = mock.ec2.Vpc 'vpc[3]'"));
}

#[test]
fn module_prefix_only_resource_collision_stays_a_plain_typed_error() {
    let (seed, _) = resolved_single_loop_resource(None);
    let outer_origin = seed
        .loop_provenance
        .direct_origin()
        .expect("seed loop origin")
        .clone();
    let current_prefix = ResourceIdentity::new("network_0123456789abcdef");
    let make_resource = || {
        let mut resource = Resource::pending_with_provider("mock", "ec2.Vpc", None).with_attribute(
            "identity_input",
            Value::Concrete(ConcreteValue::String("same".into())),
        );
        resource.module_source = Some(live_module_source("network", current_prefix.as_str()));
        resource
            .loop_provenance
            .push_module_prefix(LoopModulePrefixOrigin::new(
                outer_origin.clone(),
                current_prefix.clone(),
            ));
        resource
    };
    let mut resources = vec![make_resource(), make_resource()];

    let error = compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &loop_identity_registry(true),
        &|_| Vec::new(),
    )
    .expect_err("the conflicting declarations inside the module must fail");

    assert!(
        matches!(error, AnonymousIdentityError::Collision(_)),
        "module-prefix-only provenance must not impersonate a direct loop member: {error:?}",
    );
    assert!(!error.to_string().contains("produced by the unbound `for`"));
}

#[test]
fn nested_loop_rewrite_uses_current_module_prefix_and_is_a_valid_move_target() {
    let mut resources = parse_loop_fixture(
        "for value in ['same', 'same'] {\n  mock.ec2.Vpc { identity_input = value }\n}",
        "modules/network/main.crn",
    );
    let current_parent = ResourceIdentity::new("network_0123456789abcdef");
    let legacy_parent = LegacyLoopAddress::new("_for0[0]");
    for resource in &mut resources {
        resource
            .loop_provenance
            .prefix_for_module(&current_parent, &legacy_parent);
        resource.module_source = Some(live_module_source("network", current_parent.as_str()));
    }

    let error = compute_anonymous_identifiers_for_test(
        &mut resources,
        &[],
        &loop_identity_registry(true),
        &|_| Vec::new(),
    )
    .expect_err("nested loop collision must produce a rewrite");
    let rendered = error.to_string();
    let expected_target = "network_0123456789abcdef.vpc[0]";
    assert!(
        rendered.contains(&format!("to = mock.ec2.Vpc '{expected_target}'")),
        "rewrite used the legacy parent instead of the current instance prefix: {rendered}",
    );
    assert!(!rendered.contains("to = mock.ec2.Vpc '_for0[0].vpc[0]'"));

    let parsed = crate::parser::parse(
        &format!(
            "moved {{\n  from = mock.ec2.Vpc 'old'\n  to = mock.ec2.Vpc '{expected_target}'\n}}\n"
        ),
        &crate::parser::ProviderContext::default(),
    )
    .expect("rendered move target must parse");
    assert!(
        check_legacy_loop_move_targets::<ModuleSource>(&parsed.state_blocks, &[] as &[Resource],)
            .is_empty(),
        "the suggested current-prefix target must not be classified as legacy",
    );
}

#[test]
fn loop_conflict_suggestion_reserves_sibling_file_value_bindings() {
    let fixture = tempfile::tempdir().expect("directory fixture");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        "let vpc = 'reserved'\n",
    )
    .expect("write sibling binding");
    std::fs::write(
        fixture.path().join("main.crn"),
        "for cidr in ['10.0.0.0/16', '10.1.0.0/16'] {\n  mock.ec2.Vpc { identity_input = cidr }\n}\n",
    )
    .expect("write loop fixture");
    let parsed = crate::config_loader::parse_directory(
        fixture.path(),
        &crate::parser::ProviderContext::default(),
    )
    .expect("directory parses");
    let mut parsed = crate::module_resolver::resolve_modules_providerless(parsed, fixture.path())
        .expect("directory fixture crosses the module-resolution seam")
        .into_expanded()
        .expect("directory fixture contains no parser placeholders");
    let registry = loop_identity_registry(false);

    let error =
        compute_anonymous_identifiers_for_test(&mut parsed.resources, &[], &registry, &|_| {
            Vec::new()
        })
        .expect_err("mutable-derived loop peers require a binding");

    assert!(
        error.to_string().contains("let vpc_2 = for cidr"),
        "sibling value binding was not reserved: {error}",
    );
}

#[test]
fn loop_conflict_suggestion_reserves_later_value_bindings() {
    let source = "for cidr in ['10.0.0.0/16', '10.1.0.0/16'] {\n  mock.ec2.Vpc { identity_input = cidr }\n}\nlet vpc = 'reserved'\n";
    let mut resources = parse_loop_fixture(source, "later-binding.crn");
    let registry = loop_identity_registry(false);

    let error =
        compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
            .expect_err("mutable-derived loop peers require a binding");

    assert!(
        error.to_string().contains("let vpc_2 = for cidr"),
        "later value binding was not reserved: {error}",
    );
}

#[test]
fn multiple_collision_groups_choose_a_deterministic_error() {
    let registry = loop_identity_registry(true);
    let resource = |value: &str| {
        Resource::pending_with_provider("mock", "ec2.Vpc", None).with_attribute(
            "identity_input",
            Value::Concrete(ConcreteValue::String(value.into())),
        )
    };
    let fixture = vec![
        resource("alpha"),
        resource("alpha"),
        resource("beta"),
        resource("beta"),
    ];
    let mut messages = HashSet::new();

    for _ in 0..64 {
        let mut resources = fixture.clone();
        let error =
            compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
                .expect_err("both pairs collide");
        messages.insert(error.to_string());
    }

    assert_eq!(
        messages.len(),
        1,
        "HashMap iteration selected different collision groups: {messages:#?}",
    );
}

fn resolved_single_loop_resource(provider_instance: Option<&str>) -> (Resource, SchemaRegistry) {
    let source = "for value in ['stable'] {\n  mock.ec2.Vpc {\n    identity_input = value\n  }\n}";
    let mut resources = parse_loop_fixture(source, "legacy.crn");
    resources[0].id.provider_instance = provider_instance.map(str::to_string);
    let registry = loop_identity_registry(true);
    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("single loop element has a stable anonymous identity");
    (resources.remove(0), registry)
}

#[test]
fn legacy_loop_detector_exact_match() {
    let (resource, registry) = resolved_single_loop_resource(None);
    let claims = StateBlockClaims::empty();
    let moves = detect_legacy_loop_moves_for_test(
        std::slice::from_ref(&resource),
        &registry,
        &[loop_state_entry(
            &registry,
            "_for0[0]",
            None,
            Some("stable"),
        )],
        &claims,
        &[],
    );

    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].from().identity_str(), "_for0[0]");
    assert_eq!(
        moves[0].to().identity_str(),
        resource.id.identity_str().expect("resolved destination")
    );
}

#[test]
fn legacy_loop_detector_treats_empty_create_only_struct_as_unconfigured() {
    let source = concat!(
        "for value in ['stable'] {\n",
        "  mock.ec2.Vpc {\n",
        "    identity_input = value\n",
        "    config = {}\n",
        "  }\n",
        "}",
    );
    let mut resources = parse_loop_fixture(source, "legacy-empty-config.crn");
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        PatternResourceSchema::new("ec2.Vpc")
            .attribute(
                PatternAttributeSchema::optional("identity_input", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "config",
                    PatternAttributeType::struct_(
                        "Config",
                        vec![
                            PatternStructField::optional("label", PatternAttributeType::string())
                                .build(),
                        ],
                    ),
                )
                .create_only()
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid"),
    );
    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("single loop element must have a stable anonymous identity");
    let state = anonymous_state_entry(
        &registry,
        "mock",
        "ec2.Vpc",
        "_for0[0]",
        None,
        None,
        [
            ("identity_input", concrete_string("stable")),
            (
                "config",
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            ),
        ],
    );

    let moves = detect_legacy_loop_moves_for_test(
        &resources,
        &registry,
        &[state],
        &StateBlockClaims::empty(),
        &[],
    );

    assert_eq!(
        moves.len(),
        1,
        "an absent-equivalent create-only value must not veto legacy-row migration"
    );
}

#[test]
fn legacy_loop_detector_ignores_extra_persisted_provider_defaults() {
    let (resource, registry) = resolved_single_loop_resource(None);
    let state = anonymous_state_entry(
        &registry,
        "mock",
        "ec2.Vpc",
        "_for0[0]",
        None,
        None,
        [
            ("identity_input", concrete_string("stable")),
            ("provider_default", concrete_string("defaulted")),
        ],
    );

    let moves = detect_legacy_loop_moves_for_test(
        std::slice::from_ref(&resource),
        &registry,
        &[state],
        &StateBlockClaims::empty(),
        &[],
    );

    assert_eq!(
        moves.len(),
        1,
        "desired stable inputs uniquely identify the row"
    );
}

#[test]
fn legacy_loop_detector_refuses_a_mismatching_desired_input() {
    let (resource, registry) = resolved_single_loop_resource(None);
    let moves = detect_legacy_loop_moves_for_test(
        &[resource],
        &registry,
        &[loop_state_entry(
            &registry,
            "_for0[0]",
            None,
            Some("different"),
        )],
        &StateBlockClaims::empty(),
        &[],
    );

    assert!(
        moves.is_empty(),
        "a mismatching authored value is negative evidence"
    );
}

#[test]
fn legacy_loop_detector_compares_concrete_subset_when_other_inputs_are_references() {
    let source = "for cidr in ['10.0.0.0/24'] {\n  mock.ec2.Subnet {\n    cidr_block = cidr\n    vpc_id = 'placeholder'\n  }\n}";
    let mut resources = parse_loop_fixture(source, "subnet.crn");
    resources[0].set_attr(
        "vpc_id",
        Value::root_resource_ref("network", "vpc_id", Vec::new()),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        PatternResourceSchema::new("ec2.Subnet")
            .attribute(
                PatternAttributeSchema::optional("vpc_id", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("subnet identity schema is valid"),
    );
    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("the reference-valued input still participates in identity hashing");
    let state = anonymous_state_entry(
        &registry,
        "mock",
        "ec2.Subnet",
        "_for0[0]",
        None,
        None,
        [
            ("vpc_id", concrete_string("vpc-123")),
            ("cidr_block", concrete_string("10.9.0.0/24")),
        ],
    );

    let moves = detect_legacy_loop_moves_for_test(
        &resources,
        &registry,
        &[state],
        &StateBlockClaims::empty(),
        &[],
    );

    assert!(
        moves.is_empty(),
        "the concrete CIDR mismatch is negative evidence even though vpc_id is unresolved",
    );
}

#[test]
fn legacy_loop_detector_matches_generated_value_against_desired_prefix() {
    let mut resources = parse_loop_fixture(
        "for value in ['stable'] {\n  mock.ec2.Vpc { identity_input = value }\n}",
        "prefix.crn",
    );
    resources[0]
        .prefixes
        .insert("identity_input".to_string(), "service-".to_string());
    resources[0].set_attr(
        "identity_input",
        Value::Concrete(ConcreteValue::String("service-generated".into())),
    );
    let registry = loop_identity_registry(true);
    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("prefix-backed anonymous identity computes");

    let moves = detect_legacy_loop_moves_for_test(
        &resources,
        &registry,
        &[loop_state_entry(
            &registry,
            "_for0[0]",
            None,
            Some("service-existing"),
        )],
        &StateBlockClaims::empty(),
        &[],
    );

    assert_eq!(
        moves.len(),
        1,
        "a concrete persisted value satisfies Prefix(service-)"
    );
}

#[test]
fn moved_to_rejects_a_legacy_segment_below_a_module_prefix() {
    let (mut resource, _) = resolved_single_loop_resource(None);
    resource
        .loop_provenance
        .direct_origin_mut()
        .expect("loop origin")
        .prefix_legacy(&ResourceIdentity::new("net"));
    resource.id =
        ResourceId::with_provider_identity("mock", "ec2.Vpc", "net.mock_ec2_vpc_current", None);
    let parsed = crate::parser::parse(
        "moved {\n  from = mock.ec2.Vpc 'old'\n  to = mock.ec2.Vpc 'net._for0[0]'\n}\n",
        &crate::parser::ProviderContext::default(),
    )
    .expect("moved block parses");

    let errors = check_legacy_loop_move_targets(&parsed.state_blocks, &[resource]);

    assert_eq!(errors.len(), 1);
    assert!(errors[0].to_string().contains("net._for0[0]"));
}

#[test]
fn import_to_rejects_a_legacy_loop_address_with_the_current_replacement() {
    let (resource, _) = resolved_single_loop_resource(None);
    let current = resource
        .id
        .identity_str()
        .expect("resolved anonymous identity")
        .to_string();
    let parsed = crate::parser::parse(
        "import {\n  to = mock.ec2.Vpc '_for0[0]'\n  id = 'vpc-existing'\n}\n",
        &crate::parser::ProviderContext::default(),
    )
    .expect("import block parses");

    let errors = check_legacy_loop_move_targets(&parsed.state_blocks, &[resource]);

    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].to_string(),
        format!(
            "The `import.to` address mock.ec2.Vpc '_for0[0]' is a legacy synthetic loop address and cannot be a destination. Use the current anonymous address mock.ec2.Vpc '{current}' instead."
        )
    );
}

#[test]
fn state_block_destinations_allow_deferred_bound_discard_addresses() {
    let parsed = crate::parser::parse(
        r#"
let _ = for item in upstream.items {
  mock.ec2.Vpc { identity_input = item }
}
moved {
  from = mock.ec2.Vpc 'old'
  to = mock.ec2.Vpc '_[0]'
}
import {
  to = mock.ec2.Vpc '_[1]'
  id = 'vpc-existing'
}
"#,
        &crate::parser::ProviderContext::default(),
    )
    .expect("bound discard loop and state blocks parse");
    assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);
    assert_eq!(
        parsed.parsed_deferred_for_expressions()[0]
            .target()
            .bound_binding(),
        Some("_")
    );

    let errors =
        check_legacy_loop_move_targets::<ModuleSource>(&parsed.state_blocks, &parsed.resources);

    assert!(
        errors.is_empty(),
        "the bare discard binding is not a legacy synthetic name: {errors:#?}",
    );
}

#[test]
fn moved_to_allows_bound_map_keys_that_begin_with_underscore() {
    let parsed = crate::parser::parse(
        concat!(
            "moved {\n  from = mock.ec2.Vpc 'old-one'\n",
            "  to = mock.ec2.Vpc 'items._private'\n}\n",
            "moved {\n  from = mock.ec2.Vpc 'old-two'\n",
            "  to = mock.ec2.Vpc \"items['a._b']\"\n}\n",
        ),
        &crate::parser::ProviderContext::default(),
    )
    .expect("bound map-key move destinations parse");

    let resources: Vec<Resource> = vec![
        Resource::with_provider("mock", "ec2.Vpc", "items._private", None),
        Resource::with_provider("mock", "ec2.Vpc", "items['a._b']", None),
    ];
    let errors = check_legacy_loop_move_targets(&parsed.state_blocks, &resources);

    assert!(
        errors.is_empty(),
        "real bound map-key addresses were rejected: {errors:#?}"
    );
}

#[test]
fn moved_to_rejects_an_orphaned_legacy_segment_below_a_regular_prefix() {
    let parsed = crate::parser::parse(
        "moved {\n  from = mock.ec2.Vpc 'old'\n  to = mock.ec2.Vpc 'net._for0[0]'\n}\n",
        &crate::parser::ProviderContext::default(),
    )
    .expect("moved block parses");

    let errors =
        check_legacy_loop_move_targets::<ModuleSource>(&parsed.state_blocks, &[] as &[Resource]);

    assert_eq!(errors.len(), 1);
    assert!(errors[0].to_string().contains("net._for0[0]"));
}

#[test]
fn legacy_loop_detector_skips_claimed_source() {
    let (resource, registry) = resolved_single_loop_resource(None);
    let claims = StateBlockClaims::new(
        HashSet::from([StateBlockAddress::new("mock", "ec2.Vpc", "_for0[0]")]),
        HashSet::new(),
    );
    let moves = detect_legacy_loop_moves_for_test(
        &[resource],
        &registry,
        &[loop_state_entry(
            &registry,
            "_for0[0]",
            None,
            Some("stable"),
        )],
        &claims,
        &[],
    );

    assert!(moves.is_empty());
}

#[test]
fn legacy_loop_detector_skips_existing_destination() {
    let (resource, registry) = resolved_single_loop_resource(None);
    let destination = resource
        .id
        .identity_str()
        .expect("resolved destination")
        .to_string();
    let claims = StateBlockClaims::empty();
    let moves = detect_legacy_loop_moves_for_test(
        &[resource],
        &registry,
        &[
            loop_state_entry(&registry, "_for0[0]", None, Some("stable")),
            loop_state_entry(&registry, &destination, None, Some("stable")),
        ],
        &claims,
        &[],
    );

    assert!(moves.is_empty());
}

#[test]
fn legacy_loop_detector_matches_provider_instance() {
    let (resource, registry) = resolved_single_loop_resource(Some("west"));
    let claims = StateBlockClaims::empty();
    let moves = detect_legacy_loop_moves_for_test(
        &[resource],
        &registry,
        &[
            loop_state_entry(&registry, "_for0[0]", None, Some("stable")),
            loop_state_entry(&registry, "_for0[0]", Some("west"), Some("stable")),
        ],
        &claims,
        &[],
    );

    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].from().provider_instance(), Some("west"));
    assert_eq!(moves[0].to().provider_instance(), Some("west"));
}

#[test]
fn legacy_loop_detector_moves_named_and_anonymous_module_leaves() {
    let (seed, _) = resolved_single_loop_resource(None);
    let outer_origin = seed
        .loop_provenance
        .direct_origin()
        .expect("seed loop origin")
        .clone();
    let current_prefix = ResourceIdentity::new("web_0123456789abcdef");
    let provenance = || {
        let mut provenance = LoopProvenance::default();
        provenance.push_module_prefix(LoopModulePrefixOrigin::new(
            outer_origin.clone(),
            current_prefix.clone(),
        ));
        provenance
    };

    let mut named =
        Resource::with_provider("mock", "test.NamedLeaf", "web_0123456789abcdef.named", None);
    named.binding = Some(absolute_binding("named"));
    named.loop_provenance = provenance();
    let mut anonymous = Resource::with_provider(
        "mock",
        "test.AnonymousLeaf",
        "web_0123456789abcdef.mock_test_anonymous_leaf_12345678",
        None,
    );
    anonymous.loop_provenance = provenance();

    let claims = StateBlockClaims::empty();
    let moves = detect_legacy_loop_moves_for_test(
        &[named, anonymous],
        &SchemaRegistry::new(),
        &[
            anonymous_state_entry(
                &SchemaRegistry::new(),
                "mock",
                "test.NamedLeaf",
                "_for0[0].named",
                None,
                None,
                std::iter::empty::<(&str, Value)>(),
            ),
            anonymous_state_entry(
                &SchemaRegistry::new(),
                "mock",
                "test.AnonymousLeaf",
                "_for0[0].mock_test_anonymous_leaf_12345678",
                None,
                None,
                std::iter::empty::<(&str, Value)>(),
            ),
        ],
        &claims,
        &[],
    );

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![
            (
                "_for0[0].named".to_string(),
                "web_0123456789abcdef.named".to_string(),
            ),
            (
                "_for0[0].mock_test_anonymous_leaf_12345678".to_string(),
                "web_0123456789abcdef.mock_test_anonymous_leaf_12345678".to_string(),
            ),
        ]
    );
}

#[test]
fn legacy_loop_detector_breaks_ambiguous_owner_with_exact_stable_inputs() {
    let source_a =
        "let xs = ['alpha']\nfor value in xs {\n  mock.ec2.Vpc { identity_input = value }\n}";
    let source_b =
        "let xs = ['beta']\nfor value in xs {\n  mock.ec2.Vpc { identity_input = value }\n}";
    let mut resources = parse_loop_fixture(source_a, "a.crn");
    resources.extend(parse_loop_fixture(source_b, "b.crn"));
    let registry = loop_identity_registry(true);
    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("distinct stable inputs produce distinct destinations");
    let beta_destination = resources[1]
        .id
        .identity_str()
        .expect("resolved beta destination")
        .to_string();

    let claims = StateBlockClaims::empty();
    let moves = detect_legacy_loop_moves_for_test(
        &resources,
        &registry,
        &[loop_state_entry(&registry, "_xs[0]", None, Some("beta"))],
        &claims,
        &[],
    );

    assert_eq!(
        resolved_move_identity_pairs(&moves),
        vec![("_xs[0]".into(), beta_destination)]
    );
}

#[test]
fn legacy_loop_detector_disambiguates_with_identity_only_values() {
    let source_a =
        "let xs = ['alpha']\nfor value in xs {\n  mock.ec2.Vpc { identity_input = value }\n}";
    let source_b =
        "let xs = ['beta']\nfor value in xs {\n  mock.ec2.Vpc { identity_input = value }\n}";
    let mut resources = parse_loop_fixture(source_a, "a.crn");
    resources.extend(parse_loop_fixture(source_b, "b.crn"));
    let registry = loop_identity_only_registry();
    compute_anonymous_identifiers_for_test(&mut resources, &[], &registry, &|_| Vec::new())
        .expect("distinct identity-only inputs produce distinct destinations");
    let beta_destination = resources[1]
        .id
        .identity_str()
        .expect("resolved beta destination")
        .to_string();
    let claims = StateBlockClaims::empty();

    for (persisted_value, expected) in [
        (
            Some("beta"),
            vec![("_xs[0]".to_string(), beta_destination.clone())],
        ),
        (Some("different"), Vec::new()),
        (None, Vec::new()),
    ] {
        let moves = detect_legacy_loop_moves_for_test(
            &resources,
            &registry,
            &[loop_state_entry(&registry, "_xs[0]", None, persisted_value)],
            &claims,
            &[],
        );
        assert_eq!(
            resolved_move_identity_pairs(&moves),
            expected,
            "identity-only state case {persisted_value:?}",
        );
    }
}

#[test]
fn legacy_loop_detector_skips_ambiguous_owner_without_one_exact_input_match() {
    let source_a =
        "let xs = ['same']\nfor value in xs {\n  mock.ec2.Vpc { identity_input = value }\n}";
    let source_b =
        "let xs = ['same']\nfor value in xs {\n  mock.ec2.Vpc { identity_input = value }\n}";
    let mut resources = parse_loop_fixture(source_a, "a.crn");
    resources.extend(parse_loop_fixture(source_b, "b.crn"));
    for (index, resource) in resources.iter_mut().enumerate() {
        resource.id = crate::resource::ResourceId::with_provider_identity(
            "mock",
            "ec2.Vpc",
            ResourceIdentity::new(format!("manual_destination_{index}")),
            None,
        );
    }
    let registry = loop_identity_registry(true);
    let claims = StateBlockClaims::empty();

    for persisted_value in [Some("different"), Some("same"), None] {
        let moves = detect_legacy_loop_moves_for_test(
            &resources,
            &registry,
            &[loop_state_entry(&registry, "_xs[0]", None, persisted_value)],
            &claims,
            &[],
        );
        assert!(
            moves.is_empty(),
            "zero or multiple exact stable-input owners must not be guessed: {persisted_value:?}"
        );
    }
}
