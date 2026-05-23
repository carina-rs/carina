use super::*;
use crate::parser::{ParsedFile, ProviderContext};
use crate::resource::ManagedResource;
use crate::schema::{ResourceSchema, SchemaRegistry, TypeIdentity, noop_validator};

fn empty_parsed() -> ParsedFile {
    ParsedFile {
        providers: Vec::new(),
        resources: Vec::new(),
        data_sources: Vec::new(),
        virtual_resources: Vec::new(),
        variables: IndexMap::new(),
        uses: Vec::new(),
        module_calls: Vec::new(),
        arguments: Vec::new(),
        attribute_params: Vec::new(),
        export_params: vec![],
        backend: None,
        state_blocks: Vec::new(),
        user_functions: HashMap::new(),
        upstream_states: Vec::new(),
        wait_bindings: Vec::new(),
        requires: Vec::new(),
        structural_bindings: HashSet::new(),
        warnings: Vec::new(),
        deferred_for_expressions: Vec::new(),
    }
}

fn context_with_iam_policy_arn_validator() -> ProviderContext {
    use crate::parser::ValidatorFn;

    let mut validators: HashMap<TypeIdentity, ValidatorFn> = HashMap::new();
    validators.insert(
        TypeIdentity::bare("IamPolicyArn"),
        Box::new(|s: &str| {
            if s.starts_with("arn:aws:iam::") {
                Ok(())
            } else {
                Err(format!("invalid IAM policy ARN: '{s}'"))
            }
        }),
    );
    ProviderContext {
        decryptor: None,
        validators,
        custom_type_validator: None,
        schema_types: Default::default(),
    }
}

#[test]
fn no_bindings_no_warnings() {
    let parsed = empty_parsed();
    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn used_binding_no_warning() {
    let mut parsed = empty_parsed();

    // ManagedResource with a binding
    let vpc = ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        );
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    // ManagedResource that references the binding
    let subnet = ManagedResource::with_provider("awscc", "ec2.Subnet", "web-subnet", None)
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );
    parsed.resources.push(subnet); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn unused_binding_warns() {
    let mut parsed = empty_parsed();

    let vpc = ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        );
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let unused = check_unused_bindings(&parsed);
    assert_eq!(unused, vec!["vpc"]);
}

#[test]
fn anonymous_resource_no_warning() {
    let mut parsed = empty_parsed();

    // Anonymous resource (no _binding attribute)
    let bucket = ManagedResource::with_provider("awscc", "s3.Bucket", "my-bucket", None)
        .with_attribute(
            "bucket_name",
            Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
        );
    parsed.resources.push(bucket); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_nested_value() {
    let mut parsed = empty_parsed();

    let vpc =
        ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    // Reference inside a Map inside a List
    let mut map = IndexMap::new();
    map.insert(
        "vpc_id".to_string(),
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    let sg = ManagedResource::with_provider("awscc", "ec2.SecurityGroup", "web-sg", None)
        .with_attribute(
            "tags",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(map),
            )])),
        );
    parsed.resources.push(sg); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_module_call() {
    let mut parsed = empty_parsed();

    let vpc =
        ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let mut args = HashMap::new();
    args.insert(
        "vpc_id".to_string(),
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    parsed.module_calls.push(ModuleCall {
        module_name: "web_tier".to_string(),
        binding_name: None,
        arguments: args,
    });

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn multiple_bindings_some_unused() {
    let mut parsed = empty_parsed();

    let vpc =
        ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let sg = ManagedResource::with_provider("awscc", "ec2.SecurityGroup", "web-sg", None)
        .with_binding("web_sg");
    parsed.resources.push(sg); // allow: direct — fixture test inspection

    // Only vpc is referenced
    let subnet = ManagedResource::with_provider("awscc", "ec2.Subnet", "web-subnet", None)
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );
    parsed.resources.push(subnet); // allow: direct — fixture test inspection

    let unused = check_unused_bindings(&parsed);
    assert_eq!(unused, vec!["web_sg"]);
}

#[test]
fn binding_referenced_in_attributes_not_warned() {
    let mut parsed = empty_parsed();

    let vpc =
        ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::String),
            value: Some(Value::resource_ref(
                "vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        });

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_exports_not_warned() {
    let mut parsed = empty_parsed();

    let vpc =
        ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    parsed.export_params.push(crate::parser::ExportParameter {
        name: "vpc_id".to_string(),
        type_expr: Some(TypeExpr::String),
        value: Some(Value::resource_ref(
            "vpc".to_string(),
            "vpc_id".to_string(),
            vec![],
        )),
    });

    assert!(
        check_unused_bindings(&parsed).is_empty(),
        "binding referenced in exports should not be warned"
    );
}

#[test]
fn igw_route_crn_unused_detection() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
}

let igw = awscc.ec2.internet_gateway {
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}

let rt = awscc.ec2.RouteTable {
  vpc_id = vpc.vpc_id
}

let route = awscc.ec2.route {
  route_table_id         = rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw_attachment.internet_gateway_id
}
"#;
    let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();

    // Check the route resource's gateway_id is a ResourceRef to igw_attachment
    let route = parsed
        .resources
        .iter()
        .find(|r| r.id.name_str() == "route")
        .unwrap();
    let gateway_id = route.get_attr("gateway_id").unwrap();
    match gateway_id {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            assert_eq!(path.binding(), "igw_attachment");
            assert_eq!(path.attribute(), "internet_gateway_id");
        }
        other => panic!("Expected ResourceRef, got {:?}", other),
    }

    let unused = check_unused_bindings(&parsed);
    // igw_attachment is referenced by route, so should NOT be unused
    // route is the last resource and not referenced, so IS unused
    assert!(
        !unused.contains(&"igw_attachment".to_string()),
        "igw_attachment should not be unused"
    );
    assert_eq!(unused, vec!["route"]);
}

#[test]
fn if_expression_binding_not_warned() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let enabled = true

let vpc = if enabled {
  awscc.ec2.Vpc {
cidr_block = "10.0.0.0/16"
  }
}
"#;
    let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
    assert!(
        parsed.structural_bindings.contains("vpc"),
        "vpc should be in structural_bindings"
    );
    let unused = check_unused_bindings(&parsed);
    assert!(
        !unused.contains(&"vpc".to_string()),
        "if-expression binding should not be warned as unused"
    );
}

#[test]
fn for_expression_binding_not_warned() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpcs = for (i, env) in ["dev", "stg"] {
  awscc.ec2.Vpc {
cidr_block = cidr_subnet("10.0.0.0/8", 8, i)
  }
}
"#;
    let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
    assert!(
        parsed.structural_bindings.contains("vpcs"),
        "vpcs should be in structural_bindings"
    );
    let unused = check_unused_bindings(&parsed);
    assert!(
        unused.is_empty(),
        "for-expression bindings should not be warned as unused, got: {:?}",
        unused
    );
}

#[test]
fn read_expression_binding_not_warned() {
    let input = r#"
provider aws {
  region = aws.Region.ap_northeast_1
}

let caller = read aws.sts.caller_identity {}
"#;
    let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
    assert!(
        parsed.structural_bindings.contains("caller"),
        "caller should be in structural_bindings"
    );
    let unused = check_unused_bindings(&parsed);
    assert!(
        !unused.contains(&"caller".to_string()),
        "read-expression binding should not be warned as unused"
    );
}

#[test]
fn genuinely_unused_binding_still_warns() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
}
"#;
    let parsed = crate::parser::parse(input, &ProviderContext::default()).unwrap();
    let unused = check_unused_bindings(&parsed);
    assert_eq!(
        unused,
        vec!["vpc"],
        "genuinely unused binding should still be warned"
    );
}

#[test]
fn binding_used_inside_for_body_is_not_flagged_as_unused() {
    use crate::parser::parse;

    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        let vpc = test.r.res { name = "v" }
        for _, id in orgs.accounts {
            test.r.res {
                name = vpc.name
            }
        }
    "#;
    let parsed = parse(src, &crate::parser::ProviderContext::default()).unwrap();
    let unused = check_unused_bindings(&parsed);
    assert!(
        !unused.iter().any(|b| b == "vpc"),
        "`vpc` is referenced inside the for body, got: {unused:?}"
    );
}

/// Helper to create a simple ResourceSchema with given attributes.
fn make_schema(resource_type: &str, attrs: Vec<(&str, AttributeType)>) -> ResourceSchema {
    let mut attributes = HashMap::new();
    for (name, attr_type) in attrs {
        attributes.insert(
            name.to_string(),
            crate::schema::AttributeSchema {
                name: name.to_string(),
                attr_type,
                required: false,
                default: None,
                description: None,
                completions: None,
                provider_name: None,
                create_only: false,
                read_only: false,
                removable: None,
                block_name: None,
                write_only: false,
                identity: false,
                deferred_populate: false,
            },
        );
    }
    ResourceSchema {
        resource_type: resource_type.to_string(),
        attributes,
        description: None,
        validator: None,
        kind: crate::schema::SchemaKind::Managed,
        name_attribute: None,
        force_replace: false,
        operation_config: None,
        exclusive_required: Vec::new(),
        default_wait_timeout: None,
        default_wait_interval: None,
    }
}

#[test]
fn unknown_binding_reference_reports_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema("ec2.Subnet", vec![("vpc_id", AttributeType::String)]),
    );

    // Subnet references "vpc" binding which doesn't exist
    let subnet = ManagedResource::with_provider("awscc", "ec2.Subnet", "web-subnet", None)
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );

    let mut parsed = empty_parsed();
    parsed.resources.push(subnet); // allow: direct — fixture test inspection
    let result = validate_resource_ref_types(&parsed, &schemas, &HashSet::new());
    assert_eq!(
        result.unwrap_err(),
        "awscc.ec2.Subnet.web-subnet: unknown binding 'vpc' in reference vpc.vpc_id"
    );
}

#[test]
fn unknown_attribute_reference_reports_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema("ec2.Vpc", vec![("cidr_block", AttributeType::String)]),
    );
    schemas.insert(
        "awscc",
        make_schema("ec2.Subnet", vec![("vpc_id", AttributeType::String)]),
    );

    // VPC resource with binding
    let vpc = ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        );

    // Subnet references vpc.nonexistent_attr which doesn't exist on the VPC schema
    let subnet = ManagedResource::with_provider("awscc", "ec2.Subnet", "web-subnet", None)
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "nonexistent_attr".to_string(), vec![]),
        );

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    parsed.resources.push(subnet); // allow: direct — fixture test inspection
    let result = validate_resource_ref_types(&parsed, &schemas, &HashSet::new());
    assert_eq!(
        result.unwrap_err(),
        "awscc.ec2.Subnet.web-subnet: unknown attribute 'nonexistent_attr' on 'vpc' in reference vpc.nonexistent_attr"
    );
}

#[test]
fn unknown_attribute_reference_suggests_similar_name() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema(
            "ec2.internet_gateway",
            vec![("internet_gateway_id", AttributeType::String)],
        ),
    );
    schemas.insert(
        "awscc",
        make_schema(
            "ec2.route",
            vec![
                ("route_table_id", AttributeType::String),
                ("gateway_id", AttributeType::String),
            ],
        ),
    );

    let igw = ManagedResource::with_provider("awscc", "ec2.internet_gateway", "igw", None)
        .with_binding("igw");

    // Typo: internet_gateway_idd instead of internet_gateway_id
    let route = ManagedResource::with_provider("awscc", "ec2.route", "main-route", None)
        .with_attribute(
            "gateway_id",
            Value::resource_ref(
                "igw".to_string(),
                "internet_gateway_idd".to_string(),
                vec![],
            ),
        );

    let mut parsed = empty_parsed();
    parsed.resources.push(igw); // allow: direct — fixture test inspection
    parsed.resources.push(route); // allow: direct — fixture test inspection
    let result = validate_resource_ref_types(&parsed, &schemas, &HashSet::new());
    let err = result.unwrap_err();
    assert!(
        err.contains("Did you mean 'internet_gateway_id'?"),
        "Expected 'did you mean' suggestion, got: {}",
        err
    );
}

#[test]
fn unknown_attribute_reference_no_suggestion_when_too_different() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema("ec2.Vpc", vec![("cidr_block", AttributeType::String)]),
    );
    schemas.insert(
        "awscc",
        make_schema("ec2.Subnet", vec![("vpc_id", AttributeType::String)]),
    );

    let vpc =
        ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");

    // Completely unrelated attribute name - no suggestion expected
    let subnet = ManagedResource::with_provider("awscc", "ec2.Subnet", "web-subnet", None)
        .with_attribute(
            "vpc_id",
            Value::resource_ref(
                "vpc".to_string(),
                "completely_wrong_name".to_string(),
                vec![],
            ),
        );

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    parsed.resources.push(subnet); // allow: direct — fixture test inspection
    let result = validate_resource_ref_types(&parsed, &schemas, &HashSet::new());
    let err = result.unwrap_err();
    assert!(
        !err.contains("Did you mean"),
        "Should not suggest when name is too different, got: {}",
        err
    );
}

#[test]
fn ref_type_mismatch_inside_for_body_is_rejected() {
    // Inside a for body, assigning an Int-typed attribute to a Bool-typed
    // target must be flagged.
    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        let vpc = test.r.vpc { name = "v" }
        for _, id in orgs.xs {
            test.r.pool_user {
                pool_id = vpc.vpc_id
            }
        }
    "#;
    let parsed = crate::parser::parse(src, &ProviderContext::default()).unwrap();

    let mut schemas = SchemaRegistry::new();
    // test.r.vpc exposes `vpc_id: Int`
    schemas.insert(
        "test",
        make_schema("r.vpc", vec![("vpc_id", AttributeType::Int)]),
    );
    // test.r.pool_user requires `pool_id: Bool` — incompatible with Int.
    schemas.insert(
        "test",
        make_schema("r.pool_user", vec![("pool_id", AttributeType::Bool)]),
    );

    let result = validate_resource_ref_types(&parsed, &schemas, &HashSet::new());
    assert!(result.is_err(), "expected type-mismatch error in for body");
    let msg = result.unwrap_err();
    assert!(
        msg.contains("pool_id"),
        "expected error to mention pool_id, got: {msg}"
    );
}

#[test]
fn provider_in_module_with_arguments_errors() {
    let mut parsed = empty_parsed();
    parsed.providers.push(crate::parser::ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
    });
    parsed.arguments.push(crate::parser::ArgumentParameter {
        name: "vpc_cidr".to_string(),
        type_expr: TypeExpr::String,
        default: None,
        description: None,
        validations: Vec::new(),
    });

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "provider blocks are not allowed inside modules. Define providers at the root configuration level."
    );
}

#[test]
fn provider_in_module_with_attributes_errors() {
    let mut parsed = empty_parsed();
    parsed.providers.push(crate::parser::ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
    });
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::String),
            value: Some(Value::Concrete(ConcreteValue::String("dummy".to_string()))),
        });

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_err());
}

#[test]
fn provider_without_module_markers_ok() {
    let mut parsed = empty_parsed();
    parsed.providers.push(crate::parser::ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
    });

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_ok());
}

#[test]
fn module_without_provider_ok() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(crate::parser::ArgumentParameter {
        name: "vpc_cidr".to_string(),
        type_expr: TypeExpr::String,
        default: None,
        description: None,
        validations: Vec::new(),
    });

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_ok());
}

// --- validate_no_arguments_in_root tests ---

fn argument_param(name: &str) -> crate::parser::ArgumentParameter {
    crate::parser::ArgumentParameter {
        name: name.to_string(),
        type_expr: TypeExpr::String,
        default: None,
        description: None,
        validations: Vec::new(),
    }
}

fn provider(name: &str) -> crate::parser::ProviderConfig {
    crate::parser::ProviderConfig {
        name: name.to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
    }
}

#[test]
fn arguments_with_backend_errors() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("state_path"));
    parsed.backend = Some(crate::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes: HashMap::new(),
    });

    let result = validate_no_arguments_in_root(&parsed);
    assert_eq!(
        result.unwrap_err(),
        "arguments blocks are only valid inside module definitions, not in root configurations."
    );
}

#[test]
fn arguments_with_provider_errors() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("vpc_cidr"));
    parsed.providers.push(provider("aws"));

    let result = validate_no_arguments_in_root(&parsed);
    assert!(result.is_err());
}

#[test]
fn arguments_alone_is_ok() {
    // A directory with only `arguments` looks like a module the user
    // is validating in isolation; we cannot prove it is a root, so
    // do not flag it (issue #2198).
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("vpc_cidr"));

    let result = validate_no_arguments_in_root(&parsed);
    assert!(result.is_ok());
}

#[test]
fn empty_arguments_in_root_ok() {
    let parsed = empty_parsed();
    let result = validate_no_arguments_in_root(&parsed);
    assert!(result.is_ok());
}

// --- validate_type_expr_value tests ---

#[test]
fn validate_type_expr_value_error_uses_pascal_case() {
    let msg = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::default(),
    )
    .expect("should error");
    assert!(msg.contains("expected String"), "got: {msg}");
}

#[test]
fn validate_type_expr_struct_error_uses_pascal_case_in_field_type() {
    let mut map = indexmap::IndexMap::new();
    map.insert(
        "count".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    let fields = vec![("count".to_string(), TypeExpr::Int)];
    let msg = validate_type_expr_value(
        &TypeExpr::Struct { fields },
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::default(),
    )
    .expect("should error");
    assert!(
        msg.contains("field 'count'") && msg.contains("Int"),
        "got: {msg}"
    );
}

#[test]
fn validate_type_expr_value_ipv4_cidr_valid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv4_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv4_cidr_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv4_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("not-a-cidr".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_ipv4_address_valid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv4_address".to_string()),
        &Value::Concrete(ConcreteValue::String("192.168.1.1".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv4_address_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv4_address".to_string()),
        &Value::Concrete(ConcreteValue::String("999.999.999.999".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_ipv6_cidr_valid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("2001:db8::/32".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv6_cidr_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("not-ipv6-cidr".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_ipv6_address_valid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_address".to_string()),
        &Value::Concrete(ConcreteValue::String("2001:db8::1".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv6_address_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_address".to_string()),
        &Value::Concrete(ConcreteValue::String("zzz::zzz".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_bool_mismatch() {
    let result = validate_type_expr_value(
        &TypeExpr::Bool,
        &Value::Concrete(ConcreteValue::String("yes".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool"));
}

#[test]
fn validate_type_expr_value_int_mismatch() {
    let result = validate_type_expr_value(
        &TypeExpr::Int,
        &Value::Concrete(ConcreteValue::String("42".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int"));
}

#[test]
fn validate_type_expr_value_float_mismatch() {
    let result = validate_type_expr_value(
        &TypeExpr::Float,
        &Value::Concrete(ConcreteValue::String("3.14".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Float"));
}

#[test]
fn validate_type_expr_value_list_of_ipv4_address() {
    let items = vec![
        Value::Concrete(ConcreteValue::String("192.168.1.1".to_string())),
        Value::Concrete(ConcreteValue::String("999.0.0.1".to_string())),
    ];
    let result = validate_type_expr_value(
        &TypeExpr::List(Box::new(TypeExpr::Simple("ipv4_address".to_string()))),
        &Value::Concrete(ConcreteValue::List(items)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("Element 1"));
}

#[test]
fn validate_type_expr_value_string_type_accepts_string() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::String("hello".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_string_got_bool() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Bool(true)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got bool"));
}

#[test]
fn validate_type_expr_value_string_got_int() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got int"));
}

#[test]
fn validate_type_expr_value_string_got_float() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Float(1.5)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got float"));
}

#[test]
fn validate_type_expr_value_bool_got_int() {
    let result = validate_type_expr_value(
        &TypeExpr::Bool,
        &Value::Concrete(ConcreteValue::Int(1)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool, got int"));
}

#[test]
fn validate_type_expr_value_int_got_bool() {
    let result = validate_type_expr_value(
        &TypeExpr::Int,
        &Value::Concrete(ConcreteValue::Bool(true)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int, got bool"));
}

#[test]
fn validate_type_expr_value_float_got_bool() {
    let result = validate_type_expr_value(
        &TypeExpr::Float,
        &Value::Concrete(ConcreteValue::Bool(false)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Float, got bool"));
}

#[test]
fn validate_type_expr_value_bool_got_float_is_rejected() {
    // Regression for #2864: Float must not silently coerce into Bool.
    let result = validate_type_expr_value(
        &TypeExpr::Bool,
        &Value::Concrete(ConcreteValue::Float(1.5)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool, got float"));
}

#[test]
fn validate_type_expr_value_int_got_float_is_rejected() {
    // Regression for #2864: Float must not silently coerce into Int.
    let result = validate_type_expr_value(
        &TypeExpr::Int,
        &Value::Concrete(ConcreteValue::Float(1.5)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int, got float"));
}

#[test]
fn validate_type_expr_value_float_got_int_is_accepted() {
    // Intentional one-way widening: Int flows into Float (see #2864).
    let result = validate_type_expr_value(
        &TypeExpr::Float,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_schema_type_accepts_string() {
    let schema_type = TypeExpr::SchemaType {
        provider: "awscc".to_string(),
        path: "ec2".to_string(),
        type_name: "VpcId".to_string(),
    };
    let result = validate_type_expr_value(
        &schema_type,
        &Value::Concrete(ConcreteValue::String("vpc-12345678".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_schema_type_rejects_bool() {
    let schema_type = TypeExpr::SchemaType {
        provider: "awscc".to_string(),
        path: "ec2".to_string(),
        type_name: "VpcId".to_string(),
    };
    let result = validate_type_expr_value(
        &schema_type,
        &Value::Concrete(ConcreteValue::Bool(true)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected awscc.ec2.VpcId"));
}

#[test]
fn validate_type_expr_value_schema_type_rejects_int() {
    let schema_type = TypeExpr::SchemaType {
        provider: "awscc".to_string(),
        path: "ec2".to_string(),
        type_name: "VpcId".to_string(),
    };
    let result = validate_type_expr_value(
        &schema_type,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected awscc.ec2.VpcId"));
}

#[test]
fn discard_binding_no_warning() {
    let mut parsed = empty_parsed();

    let caller =
        ManagedResource::with_provider("aws", "sts.caller_identity", "caller_identity", None)
            .with_binding("_");
    parsed.resources.push(caller); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn validate_type_expr_custom_type_rejects_invalid() {
    let config = context_with_iam_policy_arn_validator();

    let result = validate_type_expr_value(
        &TypeExpr::Simple("iam_policy_arn".to_string()),
        &Value::Concrete(ConcreteValue::String("aaaa".to_string())),
        &config,
    );
    assert!(result.is_some(), "Expected validation error for 'aaaa'");
    assert!(result.unwrap().contains("invalid IAM policy ARN"));

    let result = validate_type_expr_value(
        &TypeExpr::Simple("iam_policy_arn".to_string()),
        &Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::123456789012:policy/MyPolicy".to_string(),
        )),
        &config,
    );
    assert!(result.is_none(), "Expected no error for valid ARN");
}

#[test]
fn validate_type_expr_list_custom_type_rejects_invalid() {
    let config = context_with_iam_policy_arn_validator();

    let result = validate_type_expr_value(
        &TypeExpr::List(Box::new(TypeExpr::Simple("iam_policy_arn".to_string()))),
        &Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("aaaa".to_string()),
        )])),
        &config,
    );
    assert!(
        result.is_some(),
        "Expected validation error for list element"
    );
    assert!(result.unwrap().contains("Element 0"));
}

fn struct_type_name_value() -> TypeExpr {
    TypeExpr::Struct {
        fields: vec![
            ("name".to_string(), TypeExpr::String),
            ("value".to_string(), TypeExpr::Int),
        ],
    }
}

#[test]
fn validate_type_expr_struct_accepts_well_formed_map() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    map.insert("value".to_string(), Value::Concrete(ConcreteValue::Int(1)));
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::default(),
    );
    assert!(result.is_none(), "got error: {:?}", result);
}

#[test]
fn validate_type_expr_struct_rejects_missing_field() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::default(),
    );
    assert_eq!(
        result.as_deref(),
        Some("expected struct, missing field 'value'.")
    );
}

#[test]
fn validate_type_expr_struct_rejects_unknown_field() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    map.insert("value".to_string(), Value::Concrete(ConcreteValue::Int(1)));
    map.insert(
        "extra".to_string(),
        Value::Concrete(ConcreteValue::String("y".to_string())),
    );
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::default(),
    );
    assert_eq!(
        result.as_deref(),
        Some("expected struct, unknown field 'extra'.")
    );
}

#[test]
fn validate_type_expr_struct_rejects_wrong_field_type() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    map.insert(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("not-an-int".to_string())),
    );
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    let msg = result.unwrap();
    assert!(msg.contains("value"), "unexpected error: {msg}");
}

#[test]
fn validate_type_expr_struct_rejects_non_map_value() {
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::String("oops".to_string())),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("struct"));
}

#[test]
fn struct_field_shape_errors_is_deterministic_for_multiple_unknowns() {
    // Multiple unknown keys must produce the *same* alphabetically-first
    // error on every call, independent of HashMap's per-process random
    // hash seed.
    let fields: Vec<(String, TypeExpr)> = vec![("a".to_string(), TypeExpr::String)];
    let mut entries: IndexMap<String, Value> = IndexMap::new();
    entries.insert(
        "a".to_string(),
        Value::Concrete(ConcreteValue::String("ok".into())),
    );
    entries.insert(
        "z_extra".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    entries.insert(
        "b_extra".to_string(),
        Value::Concrete(ConcreteValue::String("y".into())),
    );
    entries.insert(
        "m_extra".to_string(),
        Value::Concrete(ConcreteValue::String("z".into())),
    );

    let first = struct_field_shape_errors(&fields, &entries);
    for _ in 0..20 {
        assert_eq!(first, struct_field_shape_errors(&fields, &entries));
    }
    assert_eq!(
        first.as_deref(),
        Some("expected struct, unknown field 'b_extra'.")
    );
}

#[test]
fn is_type_expr_compatible_struct_rejects_missing_schema_field_when_expr_has_extra() {
    // Regression: the old `expr.iter().all(find in schema)` logic let an
    // expr struct omit a required schema field as long as sizes matched.
    // The bijection fix must reject this.
    use crate::schema::StructField;
    let expr = TypeExpr::Struct {
        fields: vec![
            ("a".to_string(), TypeExpr::Int),
            ("c".to_string(), TypeExpr::Int),
        ],
    };
    let schema = AttributeType::Struct {
        name: "Row".to_string(),
        fields: vec![
            StructField::new("a", AttributeType::Int),
            StructField::new("b", AttributeType::String),
        ],
    };
    assert!(!is_type_expr_compatible_with_schema(&expr, &schema));
}

#[test]
fn is_type_expr_compatible_struct_matches_same_shape_schema() {
    use crate::schema::StructField;
    let expr = TypeExpr::Struct {
        fields: vec![
            ("name".to_string(), TypeExpr::String),
            ("value".to_string(), TypeExpr::Int),
        ],
    };
    let schema = AttributeType::Struct {
        name: "Row".to_string(),
        fields: vec![
            StructField::new("name", AttributeType::String),
            StructField::new("value", AttributeType::Int),
        ],
    };
    assert!(is_type_expr_compatible_with_schema(&expr, &schema));
}

#[test]
fn is_type_expr_compatible_struct_flows_into_map_when_fields_share_type() {
    // A downstream consumer annotated `map(string)` accepts a
    // `struct { a: string, b: string }` — every field satisfies string.
    let expr = TypeExpr::Struct {
        fields: vec![
            ("a".to_string(), TypeExpr::String),
            ("b".to_string(), TypeExpr::String),
        ],
    };
    let schema = AttributeType::Map {
        key: Box::new(AttributeType::String),
        value: Box::new(AttributeType::String),
    };
    assert!(is_type_expr_compatible_with_schema(&expr, &schema));
}

#[test]
fn is_type_expr_compatible_struct_rejects_map_with_wrong_element_type() {
    let expr = TypeExpr::Struct {
        fields: vec![("a".to_string(), TypeExpr::String)],
    };
    let schema = AttributeType::Map {
        key: Box::new(AttributeType::String),
        value: Box::new(AttributeType::Int),
    };
    assert!(!is_type_expr_compatible_with_schema(&expr, &schema));
}

// Issue #2358: a generic `TypeExpr::String` declaration must not satisfy
// a receiver typed as `Custom { semantic_name: Some(_) }`. The receiver
// names a specific type (e.g. `VpcId`); a value carrying only the
// `String` constraint cannot prove it satisfies the more specific
// invariants the receiver demands.
#[test]
fn is_type_expr_compatible_unknown_rejects_all_concrete_receivers() {
    // Sentinel for failed inference (#2360 stage 2): never matches any
    // concrete receiver — the inference_errors channel surfaces the
    // actionable "type annotation required" instead.
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Unknown,
        &AttributeType::String,
    ));
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Unknown,
        &AttributeType::Int,
    ));
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Unknown,
        &AttributeType::Bool,
    ));
}

#[test]
fn is_type_expr_compatible_unknown_rejects_custom_receiver() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let custom = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("VpcId")),
        pattern: None,
        length: None,
        base: Box::new(AttributeType::String),
        validate: legacy_validator(noop),
    };
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Unknown,
        &custom,
    ));
}

#[test]
fn is_type_expr_compatible_string_rejects_custom_with_semantic_name() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let schema = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("VpcId")),
        pattern: None,
        length: None,
        base: Box::new(AttributeType::String),
        validate: legacy_validator(noop),
    };
    assert!(
        !is_type_expr_compatible_with_schema(&TypeExpr::String, &schema),
        "TypeExpr::String must not satisfy Custom{{semantic_name:VpcId}}"
    );
}

// Companion: a Custom receiver with no semantic_name (anonymous Custom,
// e.g. a bare `String` enriched with pattern/length but no semantic
// label) keeps accepting `TypeExpr::String` — it has no specific
// identity to demand.
#[test]
fn is_type_expr_compatible_string_accepts_custom_without_semantic_name() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let schema = AttributeType::Custom {
        identity: None,
        pattern: Some("^.+$".to_string()),
        length: None,
        base: Box::new(AttributeType::String),
        validate: legacy_validator(noop),
    };
    assert!(
        is_type_expr_compatible_with_schema(&TypeExpr::String, &schema),
        "TypeExpr::String must satisfy Custom with no semantic_name"
    );
}

// Issue #2358 Union descent: a `TypeExpr::String` declaration must not
// satisfy a `Union` receiver that contains *any* `Custom { semantic_name }`
// alternative — the value might end up flowing into the specific
// branch, and `String` cannot prove that branch's invariants.
#[test]
fn is_type_expr_compatible_string_rejects_union_containing_specific_custom() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let schema = AttributeType::Union(vec![
        AttributeType::String,
        AttributeType::Custom {
            identity: Some(TypeIdentity::bare("VpcId")),
            pattern: None,
            length: None,
            base: Box::new(AttributeType::String),
            validate: legacy_validator(noop),
        },
    ]);
    assert!(
        !is_type_expr_compatible_with_schema(&TypeExpr::String, &schema),
        "TypeExpr::String must not satisfy a Union containing Custom{{semantic}}"
    );
}

// Companion: a Union of *only* specific Custom alternatives is even
// more clearly String-incompatible — every branch demands a specific
// identity that String can't prove.
#[test]
fn is_type_expr_compatible_string_rejects_union_of_only_specific_customs() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let mk = |name: &str| AttributeType::Custom {
        identity: Some(TypeIdentity::bare(name)),
        pattern: None,
        length: None,
        base: Box::new(AttributeType::String),
        validate: legacy_validator(noop),
    };
    let schema = AttributeType::Union(vec![mk("VpcId"), mk("SubnetId")]);
    assert!(
        !is_type_expr_compatible_with_schema(&TypeExpr::String, &schema),
        "TypeExpr::String must not satisfy a Union of only specific-Custom alternatives"
    );
}

// Companion: a Union of only string-shaped, non-specific types must
// keep accepting `TypeExpr::String` (no specific-Custom alternative
// exists, so the value can safely flow into any branch).
#[test]
fn is_type_expr_compatible_string_accepts_union_of_only_strings() {
    let schema = AttributeType::Union(vec![
        AttributeType::String,
        AttributeType::StringEnum {
            name: "Mode".to_string(),
            values: vec!["A".to_string(), "B".to_string()],
            identity: None,
            dsl_aliases: vec![],
        },
    ]);
    assert!(
        is_type_expr_compatible_with_schema(&TypeExpr::String, &schema),
        "TypeExpr::String must satisfy a Union of only string-shaped non-specific types"
    );
}

// Companion: the safe direction — a specific Custom-typed export must
// continue to satisfy a receiver of the same Custom type. Pin so the
// strictness fix doesn't accidentally reject the correct direction.
#[test]
fn is_type_expr_compatible_simple_vpcid_accepts_custom_vpcid() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let schema = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("VpcId")),
        pattern: None,
        length: None,
        base: Box::new(AttributeType::String),
        validate: legacy_validator(noop),
    };
    // Parser normalizes `: VpcId` to TypeExpr::Simple("vpc_id") (snake).
    let expr = TypeExpr::Simple("vpc_id".to_string());
    assert!(is_type_expr_compatible_with_schema(&expr, &schema));
}

#[test]
fn validate_module_calls_rejects_custom_type() {
    use crate::parser::ArgumentParameter;

    let config = context_with_iam_policy_arn_validator();

    let mut args = HashMap::new();
    args.insert(
        "managed_policy_arns".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("aaaa".to_string()),
        )])),
    );

    let module_calls = vec![ModuleCall {
        module_name: "github".to_string(),
        binding_name: None,
        arguments: args,
    }];

    let mut imported_modules = HashMap::new();
    imported_modules.insert(
        "github".to_string(),
        vec![ArgumentParameter {
            name: "managed_policy_arns".to_string(),
            type_expr: TypeExpr::List(Box::new(TypeExpr::Simple("iam_policy_arn".to_string()))),
            default: None,
            description: None,
            validations: Vec::new(),
        }],
    );

    let result = validate_module_calls(&module_calls, &imported_modules, &config);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid IAM policy ARN"));
}

#[test]
fn attribute_param_ref_type_mismatch_detected() {
    use crate::parser::AttributeParameter;
    use crate::schema::{AttributeSchema, ResourceSchema};

    // Build a resource with schema: role_name is String, arn is IamRoleArn (Custom)
    let role = ManagedResource::with_provider("awscc", "iam.role", "github-role", None)
        .with_binding("role")
        .with_attribute(
            "role_name",
            Value::Concrete(ConcreteValue::String("my-role".to_string())),
        )
        .with_attribute(
            "arn",
            Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:role/my-role".to_string(),
            )),
        );

    let mut role_schema = ResourceSchema::new("iam.role");
    role_schema = role_schema.attribute(AttributeSchema::new("role_name", AttributeType::String));
    role_schema = role_schema.attribute(AttributeSchema::new(
        "arn",
        AttributeType::Custom {
            identity: Some(TypeIdentity::bare("IamRoleArn")),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
        },
    ));

    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", role_schema);

    let resources = vec![role];

    // Attribute param: role_arn: iam_role_arn = role.role_name (MISMATCH: String vs iam_role_arn)
    let params_mismatch = vec![AttributeParameter {
        name: "role_arn".to_string(),
        type_expr: Some(TypeExpr::Simple("iam_role_arn".to_string())),
        value: Some(Value::resource_ref(
            "role".to_string(),
            "role_name".to_string(),
            vec![],
        )),
    }];

    let result = validate_attribute_param_ref_types(&params_mismatch, &resources, &schemas);
    assert!(
        result.is_err(),
        "Should reject String assigned to iam_role_arn"
    );
    let err = result.unwrap_err();
    assert!(err.contains("type mismatch"), "Error: {err}");
    assert!(err.contains("iam_role_arn"), "Error: {err}");

    // Attribute param: role_arn: iam_role_arn = role.arn (MATCH: IamRoleArn matches iam_role_arn)
    let params_match = vec![AttributeParameter {
        name: "role_arn".to_string(),
        type_expr: Some(TypeExpr::Simple("iam_role_arn".to_string())),
        value: Some(Value::resource_ref(
            "role".to_string(),
            "arn".to_string(),
            vec![],
        )),
    }];

    let result = validate_attribute_param_ref_types(&params_match, &resources, &schemas);
    assert!(
        result.is_ok(),
        "Should accept IamRoleArn assigned to iam_role_arn"
    );
}

#[test]
fn validate_export_params_rejects_invalid_custom_type() {
    use crate::parser::InferredExportParam;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![InferredExportParam {
        name: "policy".to_string(),
        type_expr: TypeExpr::Simple("iam_policy_arn".to_string()),
        value: Some(Value::Concrete(ConcreteValue::String(
            "not-an-arn".to_string(),
        ))),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'policy'"), "err={err}");
    assert!(err.contains("invalid IAM policy ARN"), "err={err}");
}

#[test]
fn validate_export_params_rejects_invalid_list_element() {
    use crate::parser::InferredExportParam;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![InferredExportParam {
        name: "policies".to_string(),
        type_expr: TypeExpr::List(Box::new(TypeExpr::Simple("iam_policy_arn".to_string()))),
        value: Some(Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:policy/valid".to_string(),
            )),
            Value::Concrete(ConcreteValue::String("garbage".to_string())),
        ]))),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'policies'"), "err={err}");
    assert!(err.contains("Element 1"), "err={err}");
}

#[test]
fn validate_export_params_accepts_valid_values() {
    use crate::parser::InferredExportParam;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![InferredExportParam {
        name: "policy".to_string(),
        type_expr: TypeExpr::Simple("iam_policy_arn".to_string()),
        value: Some(Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::123456789012:policy/admin".to_string(),
        ))),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_ok());
}

#[test]
fn validate_export_params_skips_unknown_sentinel() {
    use crate::parser::InferredExportParam;

    // Stage 2 (#2360): exports with `TypeExpr::Unknown` are skipped —
    // the loader's `inference_errors` channel reports the missing
    // annotation, so re-checking here would double-report.
    let config = ProviderContext::default();
    let exports = vec![InferredExportParam {
        name: "raw".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Concrete(ConcreteValue::String(
            "anything".to_string(),
        ))),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_ok());
}

#[test]
fn validate_export_params_rejects_type_mismatch() {
    use crate::parser::InferredExportParam;

    let config = ProviderContext::default();
    let exports = vec![InferredExportParam {
        name: "flag".to_string(),
        type_expr: TypeExpr::Bool,
        value: Some(Value::Concrete(ConcreteValue::String(
            "not-a-bool".to_string(),
        ))),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'flag'"), "err={err}");
    assert!(err.contains("expected Bool"), "err={err}");
}

#[test]
fn type_compat_subtype_accepted() {
    // arn accepts KmsKeyArn (subtype via base chain: KmsKeyArn → Arn)
    let kms_key_arn = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("KmsKeyArn")),
        base: Box::new(AttributeType::Custom {
            identity: Some(TypeIdentity::bare("Arn")),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
    };
    assert!(is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("arn".to_string()),
        &kms_key_arn,
    ));
}

#[test]
fn type_compat_sibling_rejected() {
    // kms_key_arn rejects IamRoleArn (sibling: IamRoleArn → Arn, not KmsKeyArn)
    let iam_role_arn = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("IamRoleArn")),
        base: Box::new(AttributeType::Custom {
            identity: Some(TypeIdentity::bare("Arn")),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
    };
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("kms_key_arn".to_string()),
        &iam_role_arn,
    ));
}

#[test]
fn type_compat_resource_id_subtype() {
    // aws_resource_id accepts VpcId (subtype)
    let vpc_id = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("VpcId")),
        base: Box::new(AttributeType::Custom {
            identity: Some(TypeIdentity::bare("AwsResourceId")),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
    };
    assert!(is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("aws_resource_id".to_string()),
        &vpc_id,
    ));
}

#[test]
fn type_compat_resource_id_siblings_rejected() {
    // vpc_id rejects SubnetId (sibling)
    let subnet_id = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("SubnetId")),
        base: Box::new(AttributeType::Custom {
            identity: Some(TypeIdentity::bare("AwsResourceId")),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
    };
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("vpc_id".to_string()),
        &subnet_id,
    ));
}

#[test]
fn type_compat_exact_match() {
    let arn = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("Arn")),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: noop_validator(),
    };
    assert!(is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("arn".to_string()),
        &arn,
    ));
}

#[test]
fn type_compat_simple_rejected_by_mixed_string_int_union_receiver() {
    // The subtyping branch only fires when *every* member of a
    // `Union` receiver is plain String. A receiver typed
    // `Union<[String, Int]>` cannot accept a `Simple(name)` value
    // because the Int member would silently reinterpret the data.
    let mixed = AttributeType::Union(vec![AttributeType::String, AttributeType::Int]);
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("aws_account_id".to_string()),
        &mixed,
    ));
}

#[test]
fn type_compat_simple_subtypes_into_plain_string() {
    // `Simple("aws_account_id")` is a particular kind of string;
    // the plain-`String` receiver wants any string, so the value
    // satisfies it. The reverse direction (plain `String` value
    // into a `Custom { semantic_name: AwsAccountId }` receiver) is
    // rejected by `attr_type_demands_specific_custom`. See #1874
    // and #2643.
    assert!(is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("aws_account_id".to_string()),
        &AttributeType::String,
    ));
}

// Issue #2663: a semantic-ARN refinement (`IamOidcProviderArn`) flowed
// into a `Union<Struct(Principal), String>` receiver — the canonical
// shape for IAM-style policy `principal` slots — was rejected because
// the existing `Union<plain String>` rule required *every* member to
// be a plain String. The Struct member made the union fall through to
// the catch-all reject. The Simple value is unambiguously string-shaped
// at runtime, and the Struct member only accepts map-shaped values, so
// the two members are shape-disjoint and the assignment is safe.
#[test]
fn type_compat_simple_into_union_struct_or_string() {
    use crate::schema::StructField;
    let principal_union = AttributeType::Union(vec![
        AttributeType::Struct {
            name: "IamPolicyPrincipal".to_string(),
            fields: vec![StructField::new("federated", AttributeType::String)],
        },
        AttributeType::String,
    ]);
    // Issue #2663 enumerates the full set of ARN refinements that
    // should reach the principal slot; pin each one against the same
    // receiver so the table is the spec.
    for name in [
        "arn",
        "iam_role_arn",
        "iam_policy_arn",
        "iam_oidc_provider_arn",
        "kms_key_arn",
    ] {
        assert!(
            is_type_expr_compatible_with_schema(
                &TypeExpr::Simple(name.to_string()),
                &principal_union,
            ),
            "Simple({name}) should be assignable to Union<Struct, String>"
        );
    }
}

// Guards the boundary the new rule must not cross: if any non-String
// member of the union is *also* scalar-shaped, the union receiver can
// reinterpret the value down a non-string branch, which is the unsafe
// case the original `Union<String, Int>` test pinned. A union mixing
// Struct, String, and Int must still reject `Simple(name)` because the
// Int branch competes for primitive values.
#[test]
fn type_compat_simple_rejected_when_union_has_other_scalar() {
    use crate::schema::StructField;
    let mixed = AttributeType::Union(vec![
        AttributeType::Struct {
            name: "Principal".to_string(),
            fields: vec![StructField::new("federated", AttributeType::String)],
        },
        AttributeType::String,
        AttributeType::Int,
    ]);
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("iam_oidc_provider_arn".to_string()),
        &mixed,
    ));
}

// A union without any plain-String member cannot accept a Simple value:
// even a List<String> member shapes its values as a list, not a string.
#[test]
fn type_compat_simple_rejected_when_union_has_no_plain_string() {
    use crate::schema::StructField;
    let no_string = AttributeType::Union(vec![
        AttributeType::Struct {
            name: "Principal".to_string(),
            fields: vec![StructField::new("federated", AttributeType::String)],
        },
        AttributeType::List {
            inner: Box::new(AttributeType::String),
            ordered: true,
        },
    ]);
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("iam_oidc_provider_arn".to_string()),
        &no_string,
    ));
}

// `Custom` and `StringEnum` are deliberately excluded from the new
// Union allow-list: both are string-shaped at runtime, so a `Simple`
// value sharing a union with one of them is ambiguous about which
// branch the consumer treats as the route. The Custom-chain walk
// above already handles the *specific* "Simple subtypes of this
// Custom" case at the top of the arm; the union escape hatch is for
// shape-disjoint members only. If these arms are ever folded into the
// allow-list, this test will fail and force a re-think.
#[test]
fn type_compat_simple_rejected_when_union_has_string_shaped_peer() {
    let arn = AttributeType::Custom {
        identity: Some(TypeIdentity::bare("Arn")),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: noop_validator(),
    };
    let with_custom = AttributeType::Union(vec![AttributeType::String, arn]);
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("iam_oidc_provider_arn".to_string()),
        &with_custom,
    ));
    let with_enum = AttributeType::Union(vec![
        AttributeType::String,
        AttributeType::StringEnum {
            name: "Status".to_string(),
            values: vec!["enabled".to_string(), "disabled".to_string()],
            identity: None,
            dsl_aliases: vec![],
        },
    ]);
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("iam_oidc_provider_arn".to_string()),
        &with_enum,
    ));
}

#[test]
fn validate_export_param_ref_types_map_accepts_compatible_types() {
    use crate::parser::InferredExportParam;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema(
            "organizations.account",
            vec![("account_id", AttributeType::String)],
        ),
    );

    let registry_prod =
        ManagedResource::with_provider("awscc", "organizations.account", "prod", None)
            .with_binding("registry_prod")
            .with_attribute(
                "account_id",
                Value::Concrete(ConcreteValue::String("111".to_string())),
            );

    let mut map_value = IndexMap::new();
    map_value.insert(
        "prod".to_string(),
        Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        ),
    );

    let exports = vec![InferredExportParam {
        name: "accounts".to_string(),
        // declared as map(string), and values are String-typed — should pass
        type_expr: TypeExpr::Map(Box::new(TypeExpr::String)),
        value: Some(Value::Concrete(ConcreteValue::Map(map_value))),
    }];

    let result = validate_export_param_ref_types(&exports, &[registry_prod], &schemas);
    assert!(
        result.is_ok(),
        "map(String) = {{ prod = registry_prod.account_id (String) }} should pass, got: {:?}",
        result
    );
}

#[test]
fn validate_export_param_ref_types_map_rejects_type_mismatch() {
    use crate::parser::InferredExportParam;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema(
            "organizations.account",
            vec![("account_id", AttributeType::String)],
        ),
    );

    let registry_prod =
        ManagedResource::with_provider("awscc", "organizations.account", "prod", None)
            .with_binding("registry_prod")
            .with_attribute(
                "account_id",
                Value::Concrete(ConcreteValue::String("111".to_string())),
            );

    let mut map_value = IndexMap::new();
    map_value.insert(
        "prod".to_string(),
        Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        ),
    );

    let exports = vec![InferredExportParam {
        name: "accounts".to_string(),
        // declared as map(bool) — values should be rejected as they are strings
        type_expr: TypeExpr::Map(Box::new(TypeExpr::Bool)),
        value: Some(Value::Concrete(ConcreteValue::Map(map_value))),
    }];

    let result = validate_export_param_ref_types(&exports, &[registry_prod], &schemas);
    assert!(
        result.is_err(),
        "map(Bool) = {{ prod = registry_prod.account_id }} (String) should be flagged as type mismatch"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("accounts") && err.contains("type mismatch"),
        "error should mention the export name and type mismatch, got: {err}"
    );
}

#[test]
fn validate_export_param_ref_types_skips_unknown_sentinel() {
    use crate::parser::InferredExportParam;

    // Sentinel-bearing exports are surfaced via `inference_errors`,
    // so the ref-type validator must skip them silently — emitting a
    // duplicate diagnostic here would double-report the same issue.
    let exports = vec![InferredExportParam {
        name: "zone_id".to_string(),
        type_expr: TypeExpr::Unknown,
        value: Some(Value::Concrete(ConcreteValue::String(
            "ignored".to_string(),
        ))),
    }];

    let result = validate_export_param_ref_types(&exports, &[], &SchemaRegistry::new());
    assert!(
        result.is_ok(),
        "Unknown sentinel must be skipped, got {:?}",
        result
    );
}

#[test]
fn validate_export_param_ref_types_against_inferred_inputs() {
    use crate::parser::{InferredExportParam, UpstreamState};

    // Smoke test: a happy-path post-inference shape (bare TypeExpr,
    // matching attribute type) typechecks cleanly through the new
    // signature.
    let registry_prod =
        ManagedResource::with_provider("awscc", "organizations.account", "prod", None)
            .with_binding("registry_prod")
            .with_attribute(
                "account_id",
                Value::Concrete(ConcreteValue::String("111".to_string())),
            );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema(
            "organizations.account",
            vec![("account_id", AttributeType::String)],
        ),
    );

    let exports = vec![InferredExportParam {
        name: "id".to_string(),
        type_expr: TypeExpr::String,
        value: Some(Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        )),
    }];

    let _: &[UpstreamState] = &[]; // signature no longer takes upstream_states; doc only.
    let result = validate_export_param_ref_types(&exports, &[registry_prod], &schemas);
    assert!(result.is_ok(), "got {:?}", result);
}

/// Issue #2954 acceptance. A `Value::Deferred(DeferredValue::ResourceRef)` whose receiver is a
/// `List<String>` attribute (e.g. `resource_records = registry_dev.nameservers`
/// against `aws.route53.RecordSet.resource_records: List<String>`) must
/// not be rejected by `validate_resources`. Pre-Phase-2 the schema-level
/// `validate_list` returned `Type mismatch: expected List<String>, got
/// ResourceRef(...)`. Phase 2 (RFC #2972) makes the validate dispatcher
/// project `&Value` through `as_concrete()` so deferred values like
/// `ResourceRef` cannot reach `validate_list` by construction — the
/// type-fitness check for upstream-typed refs is the deferred-aware
/// checker's job (`check_upstream_state_field_types`).
#[test]
fn validate_resources_accepts_resource_ref_in_list_position() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        make_schema(
            "route53.RecordSet",
            vec![(
                "resource_records",
                AttributeType::list(AttributeType::String),
            )],
        ),
    );

    let record_set = ManagedResource::with_provider("aws", "route53.RecordSet", "ns", None)
        .with_attribute(
            "resource_records",
            Value::resource_ref(
                "registry_dev".to_string(),
                "nameservers".to_string(),
                vec![],
            ),
        );

    let mut known = HashSet::new();
    known.insert("aws".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(record_set); // allow: direct — fixture test inspection
    let result = validate_resources(&parsed, &schemas, &known, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "ResourceRef in List<String> position must not produce a schema-level \
         type mismatch (#2954), got: {:?}",
        result
    );
}

/// Sibling of `..._in_list_position`: same invariant for a struct
/// **field** position. `collect_struct` previously skipped
/// `Value::Deferred(DeferredValue::ResourceRef)` per-field with an explicit `matches!` guard;
/// Phase 2 (RFC #2972) removed it because `collect_into` now projects
/// each field through `as_concrete()` and short-circuits on `None`.
/// This test pins the resulting behavior end-to-end.
#[test]
fn validate_resources_accepts_resource_ref_in_struct_field_position() {
    use crate::schema::StructField;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        make_schema(
            "test.StructHolder",
            vec![(
                "config",
                AttributeType::Struct {
                    name: "Config".to_string(),
                    fields: vec![StructField::new("name", AttributeType::String)],
                },
            )],
        ),
    );

    let mut config = IndexMap::new();
    config.insert(
        "name".to_string(),
        Value::resource_ref("vpc".to_string(), "name".to_string(), vec![]),
    );

    let holder = ManagedResource::with_provider("aws", "test.StructHolder", "h", None)
        .with_attribute("config", Value::Concrete(ConcreteValue::Map(config)));

    let mut known = HashSet::new();
    known.insert("aws".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(holder); // allow: direct — fixture test inspection
    let result = validate_resources(&parsed, &schemas, &known, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "ResourceRef in Struct field position must not produce a schema-level type \
         mismatch (#2954), got: {:?}",
        result
    );
}

/// Sibling of `..._in_list_position`: same invariant for `Map<K,V>`.
#[test]
fn validate_resources_accepts_resource_ref_in_map_position() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        make_schema(
            "test.MapHolder",
            vec![(
                "tags",
                AttributeType::Map {
                    key: Box::new(AttributeType::String),
                    value: Box::new(AttributeType::String),
                },
            )],
        ),
    );

    let holder = ManagedResource::with_provider("aws", "test.MapHolder", "h", None).with_attribute(
        "tags",
        Value::resource_ref("orgs".to_string(), "tag_map".to_string(), vec![]),
    );

    let mut known = HashSet::new();
    known.insert("aws".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(holder); // allow: direct — fixture test inspection
    let result = validate_resources(&parsed, &schemas, &known, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "ResourceRef in Map position must not produce a schema-level type mismatch \
         (#2954), got: {:?}",
        result
    );
}

/// `validate_resources` must reject a resource whose schema declares an
/// `exclusive_required` group that is not satisfied — mirrors
/// `awscc.ec2.Vpc {}` with no cidr_block / ipam pool.
#[test]
fn validate_resources_rejects_missing_exclusive_required() {
    let schema = make_schema(
        "ec2.Vpc",
        vec![
            ("cidr_block", AttributeType::String),
            ("ipv4_ipam_pool_id", AttributeType::String),
        ],
    )
    .exclusive_required(&["cidr_block", "ipv4_ipam_pool_id"]);

    let mut schemas = SchemaRegistry::new();
    schemas.insert("awscc", schema);

    let vpc = ManagedResource::with_provider("awscc", "ec2.Vpc", "main-vpc", None);

    let mut known = HashSet::new();
    known.insert("awscc".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    let err =
        validate_resources(&parsed, &schemas, &known, &ProviderContext::default()).unwrap_err();
    assert!(
        err.contains("Exactly one of [cidr_block, ipv4_ipam_pool_id] must be specified"),
        "expected exclusive_required error, got: {err}"
    );
}

#[test]
fn enum_membership_violation_in_for_body_is_flagged() {
    // Regression for #2044: inside a `for` body, a string literal that
    // isn't a valid member of a StringEnum attribute must be flagged.
    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        for _, id in orgs.xs {
            test.r.mode_holder {
                mode = "aaaa"
            }
        }
    "#;
    let parsed = crate::parser::parse(src, &ProviderContext::default()).unwrap();

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        make_schema(
            "r.mode_holder",
            vec![(
                "mode",
                AttributeType::StringEnum {
                    name: "Mode".to_string(),
                    values: vec!["on".to_string(), "off".to_string()],
                    identity: None,
                    dsl_aliases: vec![],
                },
            )],
        ),
    );

    let mut known = HashSet::new();
    known.insert("test".to_string());

    let result = validate_resources(&parsed, &schemas, &known, &ProviderContext::default());
    assert!(result.is_err(), "expected enum-mismatch error in for body");
    let err = result.unwrap_err();
    assert!(
        err.contains("aaaa"),
        "expected error to mention 'aaaa', got: {err}"
    );
}

/// Tests for #2094 — the four cases the PR 2 diagnostic must distinguish:
///   1. `mode = "aaa"` (string literal)   → StringLiteralExpectedEnum
///   2. `mode = aaa`   (bare invalid)     → InvalidEnumVariant (unchanged)
///   3. `mode = fast`  (bare valid)       → pass (unchanged)
///   4. fully-qualified form              → pass (unchanged)
fn mode_schema() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        make_schema(
            "r.mode_holder",
            vec![(
                "mode",
                AttributeType::StringEnum {
                    name: "Mode".to_string(),
                    values: vec!["fast".to_string(), "slow".to_string()],
                    identity: Some(crate::schema::string_enum_identity("Mode", Some("test.r"))),
                    dsl_aliases: vec![],
                },
            )],
        ),
    );
    schemas
}

fn mode_known() -> HashSet<String> {
    let mut known = HashSet::new();
    known.insert("test".to_string());
    known
}

fn parse_mode(src: &str) -> ParsedFile {
    crate::parser::parse(src, &ProviderContext::default()).unwrap()
}

#[test]
fn quoted_literal_enum_value_yields_string_literal_diagnostic() {
    // Case 1: `mode = "aaa"` — the parser tagged the path, validation
    // must surface the shape-mismatch message, not a variant list.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = "aaa"
        }
        "#,
    );
    let err = validate_resources(
        &parsed,
        &mode_schema(),
        &mode_known(),
        &ProviderContext::default(),
    )
    .unwrap_err();
    assert!(
        err.contains("got a string literal"),
        "quoted literal must emit the shape-mismatch diagnostic, got: {err}"
    );
    assert!(
        err.contains("\"aaa\""),
        "diagnostic must echo the user's literal, got: {err}"
    );
    assert!(
        err.contains("test.r.Mode.fast") || err.contains("test.r.Mode.slow"),
        "diagnostic must list fully-qualified valid variants, got: {err}"
    );
}

#[test]
fn bare_invalid_enum_value_keeps_invalid_variant_diagnostic() {
    // Case 2: `mode = aaa` — the parser did NOT tag this as a literal,
    // so the classic `InvalidEnumVariant` message must still be used.
    // Guards against regressing #2077/#2098 wording.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = aaa
        }
        "#,
    );
    let err = validate_resources(
        &parsed,
        &mode_schema(),
        &mode_known(),
        &ProviderContext::default(),
    )
    .unwrap_err();
    assert!(
        !err.contains("got a string literal"),
        "bare identifier must NOT get the shape-mismatch diagnostic, got: {err}"
    );
    assert!(
        err.contains("aaa"),
        "diagnostic must still echo the typed value, got: {err}"
    );
}

#[test]
fn bare_valid_enum_value_passes() {
    // Case 3: `mode = fast` — bare identifier resolves to a real
    // variant and must pass cleanly.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = fast
        }
        "#,
    );
    assert!(
        validate_resources(
            &parsed,
            &mode_schema(),
            &mode_known(),
            &ProviderContext::default()
        )
        .is_ok(),
        "bare valid identifier must pass"
    );
}

#[test]
fn fully_qualified_enum_value_passes() {
    // Case 4: `mode = test.r.Mode.fast` — fully-qualified form must
    // still pass unchanged.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = test.r.Mode.fast
        }
        "#,
    );
    assert!(
        validate_resources(
            &parsed,
            &mode_schema(),
            &mode_known(),
            &ProviderContext::default()
        )
        .is_ok(),
        "fully-qualified identifier must pass"
    );
}

/// `read` against a managed-only resource type emits a kind-specific error
/// pointing the user at the fix (drop the `read` keyword).
///
/// Companion to the inverse direction — non-`read` against a data-source-only
/// type — which the existing "is a data source and must be used with the
/// `read` keyword" tests already cover.
#[test]
fn read_against_managed_only_type_is_rejected() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        make_schema("ec2.Vpc", vec![("cidr_block", AttributeType::String)]),
    );

    let parsed = crate::parser::parse(
        r#"
        let v = read awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
        "#,
        &ProviderContext::default(),
    )
    .unwrap();

    let mut known = HashSet::new();
    known.insert("awscc".to_string());

    let err =
        validate_resources(&parsed, &schemas, &known, &ProviderContext::default()).unwrap_err();
    assert!(
        err.contains("is a managed resource, not a data source"),
        "expected managed-only diagnostic, got: {err}"
    );
    assert!(
        err.contains("Remove the `read` keyword"),
        "expected fix hint, got: {err}"
    );
}

#[test]
fn validate_type_expr_value_skips_value_unknown() {
    // RFC #2371 stage 3: a `Value::Deferred(DeferredValue::Unknown)` reaching this validator
    // (e.g. from a deferred-for body where a typed module-call argument
    // is bound to the loop var) must short-circuit rather than emit
    // `expected <type>, got unknown`.
    use crate::resource::{DeferredValue, UnknownReason, Value};
    use crate::validation::TypeExpr;

    let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
    let cfg = ProviderContext::default();

    assert!(super::validate_type_expr_value(&TypeExpr::String, &unknown, &cfg).is_none());
    assert!(super::validate_type_expr_value(&TypeExpr::Int, &unknown, &cfg).is_none());
    assert!(super::validate_type_expr_value(&TypeExpr::Bool, &unknown, &cfg).is_none());

    let struct_ty = TypeExpr::Struct {
        fields: vec![("name".to_string(), TypeExpr::String)],
    };
    assert!(super::validate_type_expr_value(&struct_ty, &unknown, &cfg).is_none());

    let upstream = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
        path: crate::resource::AccessPath::with_fields("net", "vpc", vec!["vpc_id".into()]),
    }));
    assert!(super::validate_type_expr_value(&TypeExpr::String, &upstream, &cfg).is_none());
    assert!(super::validate_type_expr_value(&struct_ty, &upstream, &cfg).is_none());
}

/// carina#3028: when a `ResourceRef` chains `[idx]` followed by
/// `.field`, the type checker must narrow the schema type through
/// each subscript (`List<T> → T`, `Map<_,V> → V`) before walking the
/// following field segments. Pre-fix the checker compared the whole
/// `List<Struct>` against the receiver `String`, ignoring the path
/// entirely past `binding.attribute`.
#[test]
fn ref_with_chained_subscript_then_field_narrows_through_list() {
    use crate::resource::{AccessPath, PathSegment, Subscript, Value};
    use crate::schema::StructField;

    let mut schemas = SchemaRegistry::new();
    // `aws.acm.Certificate` exposes `domain_validation_options:
    // List<Struct{resource_record_name: String, ...}>`.
    schemas.insert(
        "aws",
        make_schema(
            "acm.Certificate",
            vec![(
                "domain_validation_options",
                AttributeType::List {
                    inner: Box::new(AttributeType::Struct {
                        name: "DomainValidationOption".to_string(),
                        fields: vec![
                            StructField::new("resource_record_name", AttributeType::String),
                            StructField::new("resource_record_value", AttributeType::String),
                        ],
                    }),
                    ordered: true,
                },
            )],
        ),
    );
    // `aws.route53.RecordSet.name` is a plain String.
    schemas.insert(
        "aws",
        make_schema("route53.RecordSet", vec![("name", AttributeType::String)]),
    );

    let cert =
        ManagedResource::with_provider("aws", "acm.Certificate", "main", None).with_binding("cert");

    // `cert.domain_validation_options[0].resource_record_name`
    let path = AccessPath::with_segments(
        "cert",
        "domain_validation_options",
        vec![
            PathSegment::Subscript {
                index: Subscript::Int { index: 0 },
            },
            PathSegment::Field {
                name: "resource_record_name".to_string(),
            },
        ],
    );
    let record = ManagedResource::with_provider("aws", "route53.RecordSet", "validation", None)
        .with_attribute(
            "name",
            Value::Deferred(crate::resource::DeferredValue::ResourceRef { path }),
        );

    let mut parsed = empty_parsed();
    parsed.resources.push(cert); // allow: direct — fixture test inspection
    parsed.resources.push(record); // allow: direct — fixture test inspection
    let result = validate_resource_ref_types(&parsed, &schemas, &HashSet::new());
    assert!(
        result.is_ok(),
        "chained subscript-then-field should narrow List<Struct> → Struct → String, got: {:?}",
        result.err(),
    );
}

/// Sibling check for carina#3028: when the narrowed leaf is itself
/// incompatible with the receiver, the checker must still flag it —
/// narrowing is "honest" about the path, not a silent acceptance.
#[test]
fn ref_with_chained_subscript_then_field_rejects_real_mismatch() {
    use crate::resource::{AccessPath, PathSegment, Subscript, Value};
    use crate::schema::StructField;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        make_schema(
            "acm.Certificate",
            vec![(
                "domain_validation_options",
                AttributeType::List {
                    inner: Box::new(AttributeType::Struct {
                        name: "DomainValidationOption".to_string(),
                        fields: vec![StructField::new("rotation_count", AttributeType::Int)],
                    }),
                    ordered: true,
                },
            )],
        ),
    );
    // Receiver `name` is String — the chained access yields Int.
    schemas.insert(
        "aws",
        make_schema("route53.RecordSet", vec![("name", AttributeType::String)]),
    );

    let cert =
        ManagedResource::with_provider("aws", "acm.Certificate", "main", None).with_binding("cert");

    let path = AccessPath::with_segments(
        "cert",
        "domain_validation_options",
        vec![
            PathSegment::Subscript {
                index: Subscript::Int { index: 0 },
            },
            PathSegment::Field {
                name: "rotation_count".to_string(),
            },
        ],
    );
    let record = ManagedResource::with_provider("aws", "route53.RecordSet", "validation", None)
        .with_attribute(
            "name",
            Value::Deferred(crate::resource::DeferredValue::ResourceRef { path }),
        );

    let mut parsed = empty_parsed();
    parsed.resources.push(cert); // allow: direct — fixture test inspection
    parsed.resources.push(record); // allow: direct — fixture test inspection
    let err = validate_resource_ref_types(&parsed, &schemas, &HashSet::new()).unwrap_err();
    assert!(
        err.contains("expected String"),
        "expected real Int→String mismatch to be flagged, got: {err}"
    );
}

/// carina#3041: when a chained `[idx].field` references a struct
/// field that does not exist (typically because the user is still on
/// an older flat shape like `domain_validation_options[0].resource_record_name`
/// while the schema has migrated to a nested
/// `domain_validation_options[0].resource_record.name`), validation
/// must flag the unknown field by name with a suggestion. Pre-fix
/// `narrow_attribute_type` returned `None` silently, the caller
/// swallowed it, and the failure only surfaced at apply time with a
/// misleading "Add a `wait` block" message — `wait` could never have
/// helped, because the attribute will never exist under that spelling.
#[test]
fn ref_with_chained_field_on_struct_flags_unknown_field() {
    use crate::resource::{AccessPath, PathSegment, Subscript, Value};
    use crate::schema::StructField;

    let mut schemas = SchemaRegistry::new();
    // Mirror the real `aws.acm.Certificate` shape after aws#295:
    // `domain_validation_options[*].resource_record: Struct{name, value}`.
    // Note: NO flat `resource_record_name` / `resource_record_value`
    // fields on the inner struct — those were removed by the nested
    // migration.
    schemas.insert(
        "aws",
        make_schema(
            "acm.Certificate",
            vec![(
                "domain_validation_options",
                AttributeType::List {
                    inner: Box::new(AttributeType::Struct {
                        name: "DomainValidation".to_string(),
                        fields: vec![
                            StructField::new("domain_name", AttributeType::String),
                            StructField::new(
                                "resource_record",
                                AttributeType::Struct {
                                    name: "ResourceRecord".to_string(),
                                    fields: vec![
                                        StructField::new("name", AttributeType::String),
                                        StructField::new("value", AttributeType::String),
                                    ],
                                },
                            ),
                        ],
                    }),
                    ordered: true,
                },
            )],
        ),
    );
    schemas.insert(
        "aws",
        make_schema("route53.RecordSet", vec![("name", AttributeType::String)]),
    );

    let cert =
        ManagedResource::with_provider("aws", "acm.Certificate", "main", None).with_binding("cert");

    // User wrote `cert.domain_validation_options[0].resource_record_name`
    // — the old flat spelling that the nested-shape migration replaced
    // with `[0].resource_record.name`.
    let path = AccessPath::with_segments(
        "cert",
        "domain_validation_options",
        vec![
            PathSegment::Subscript {
                index: Subscript::Int { index: 0 },
            },
            PathSegment::Field {
                name: "resource_record_name".to_string(),
            },
        ],
    );
    let record = ManagedResource::with_provider("aws", "route53.RecordSet", "validation", None)
        .with_attribute(
            "name",
            Value::Deferred(crate::resource::DeferredValue::ResourceRef { path }),
        );

    let mut parsed = empty_parsed();
    parsed.resources.push(cert); // allow: direct — fixture test inspection
    parsed.resources.push(record); // allow: direct — fixture test inspection
    let err = validate_resource_ref_types(&parsed, &schemas, &HashSet::new()).unwrap_err();
    assert!(
        err.contains("resource_record_name"),
        "error must name the unknown field, got: {err}",
    );
    // Struct name must appear (not a fuzzy "the word 'struct'" match —
    // the diagnostic commits to naming the enclosing struct so the
    // user knows where to look).
    assert!(
        err.contains("DomainValidation"),
        "error must identify the enclosing struct, got: {err}",
    );
    // Sibling fields must be enumerated so the user can discover the
    // new spelling (`resource_record.name`) even when `suggest_similar_name`'s
    // Levenshtein threshold (3 for a 20-char identifier) doesn't fire
    // on this specific typo distance. The `known fields:` prefix pins
    // the list, not the path — the path also contains `resource_record`
    // and would otherwise produce a trivially-satisfied assertion.
    assert!(
        err.contains("known fields:"),
        "error must enumerate known fields, got: {err}"
    );
    let known_section = err.split("known fields:").nth(1).unwrap_or("");
    assert!(
        known_section.contains("resource_record"),
        "known fields list must include the renamed field, got: {err}",
    );
    assert!(
        known_section.contains("domain_name"),
        "known fields list must enumerate all siblings, got: {err}",
    );
}
