use super::*;
use crate::parser::{ParsedFile, ProviderContext};
use crate::resource::Resource;
use crate::schema::noop_validator;

fn empty_parsed() -> ParsedFile {
    ParsedFile {
        providers: Vec::new(),
        resources: Vec::new(),
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
        requires: Vec::new(),
        structural_bindings: HashSet::new(),
        warnings: Vec::new(),
        deferred_for_expressions: Vec::new(),
    }
}

fn context_with_iam_policy_arn_validator() -> ProviderContext {
    use crate::parser::ValidatorFn;

    let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
    validators.insert(
        "iam_policy_arn".to_string(),
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

    // Resource with a binding
    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    // Resource that references the binding
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet").with_attribute(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    parsed.resources.push(subnet); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn unused_binding_warns() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let unused = check_unused_bindings(&parsed);
    assert_eq!(unused, vec!["vpc"]);
}

#[test]
fn anonymous_resource_no_warning() {
    let mut parsed = empty_parsed();

    // Anonymous resource (no _binding attribute)
    let bucket = Resource::with_provider("awscc", "s3.Bucket", "my-bucket")
        .with_attribute("bucket_name", Value::String("my-bucket".to_string()));
    parsed.resources.push(bucket); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_nested_value() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc").with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    // Reference inside a Map inside a List
    let mut map = IndexMap::new();
    map.insert(
        "vpc_id".to_string(),
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    let sg = Resource::with_provider("awscc", "ec2.SecurityGroup", "web-sg")
        .with_attribute("tags", Value::List(vec![Value::Map(map)]));
    parsed.resources.push(sg); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_module_call() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc").with_binding("vpc");
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

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc").with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let sg = Resource::with_provider("awscc", "ec2.SecurityGroup", "web-sg").with_binding("web_sg");
    parsed.resources.push(sg); // allow: direct — fixture test inspection

    // Only vpc is referenced
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet").with_attribute(
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

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc").with_binding("vpc");
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

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc").with_binding("vpc");
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
        Value::ResourceRef { path } => {
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
            },
        );
    }
    ResourceSchema {
        resource_type: resource_type.to_string(),
        attributes,
        description: None,
        validator: None,
        data_source: false,
        name_attribute: None,
        force_replace: false,
        operation_config: None,
        exclusive_required: Vec::new(),
    }
}

fn test_schema_key_fn(r: &Resource) -> String {
    r.id.resource_type.clone()
}

#[test]
fn unknown_binding_reference_reports_error() {
    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.Subnet".to_string(),
        make_schema("ec2.Subnet", vec![("vpc_id", AttributeType::String)]),
    );

    // Subnet references "vpc" binding which doesn't exist
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet").with_attribute(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );

    let mut parsed = empty_parsed();
    parsed.resources.push(subnet); // allow: direct — fixture test inspection
    let result =
        validate_resource_ref_types(&parsed, &schemas, &test_schema_key_fn, &HashSet::new());
    assert_eq!(
        result.unwrap_err(),
        "awscc.ec2.Subnet.web-subnet: unknown binding 'vpc' in reference vpc.vpc_id"
    );
}

#[test]
fn unknown_attribute_reference_reports_error() {
    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.Vpc".to_string(),
        make_schema("ec2.Vpc", vec![("cidr_block", AttributeType::String)]),
    );
    schemas.insert(
        "ec2.Subnet".to_string(),
        make_schema("ec2.Subnet", vec![("vpc_id", AttributeType::String)]),
    );

    // VPC resource with binding
    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string()));

    // Subnet references vpc.nonexistent_attr which doesn't exist on the VPC schema
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet").with_attribute(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "nonexistent_attr".to_string(), vec![]),
    );

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    parsed.resources.push(subnet); // allow: direct — fixture test inspection
    let result =
        validate_resource_ref_types(&parsed, &schemas, &test_schema_key_fn, &HashSet::new());
    assert_eq!(
        result.unwrap_err(),
        "awscc.ec2.Subnet.web-subnet: unknown attribute 'nonexistent_attr' on 'vpc' in reference vpc.nonexistent_attr"
    );
}

#[test]
fn unknown_attribute_reference_suggests_similar_name() {
    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.internet_gateway".to_string(),
        make_schema(
            "ec2.internet_gateway",
            vec![("internet_gateway_id", AttributeType::String)],
        ),
    );
    schemas.insert(
        "ec2.route".to_string(),
        make_schema(
            "ec2.route",
            vec![
                ("route_table_id", AttributeType::String),
                ("gateway_id", AttributeType::String),
            ],
        ),
    );

    let igw = Resource::with_provider("awscc", "ec2.internet_gateway", "igw").with_binding("igw");

    // Typo: internet_gateway_idd instead of internet_gateway_id
    let route = Resource::with_provider("awscc", "ec2.route", "main-route").with_attribute(
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
    let result =
        validate_resource_ref_types(&parsed, &schemas, &test_schema_key_fn, &HashSet::new());
    let err = result.unwrap_err();
    assert!(
        err.contains("Did you mean 'internet_gateway_id'?"),
        "Expected 'did you mean' suggestion, got: {}",
        err
    );
}

#[test]
fn unknown_attribute_reference_no_suggestion_when_too_different() {
    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.Vpc".to_string(),
        make_schema("ec2.Vpc", vec![("cidr_block", AttributeType::String)]),
    );
    schemas.insert(
        "ec2.Subnet".to_string(),
        make_schema("ec2.Subnet", vec![("vpc_id", AttributeType::String)]),
    );

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc").with_binding("vpc");

    // Completely unrelated attribute name - no suggestion expected
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet").with_attribute(
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
    let result =
        validate_resource_ref_types(&parsed, &schemas, &test_schema_key_fn, &HashSet::new());
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

    let mut schemas = HashMap::new();
    // test.r.vpc exposes `vpc_id: Int`
    schemas.insert(
        "r.vpc".to_string(),
        make_schema("r.vpc", vec![("vpc_id", AttributeType::Int)]),
    );
    // test.r.pool_user requires `pool_id: Bool` — incompatible with Int.
    schemas.insert(
        "r.pool_user".to_string(),
        make_schema("r.pool_user", vec![("pool_id", AttributeType::Bool)]),
    );

    let result =
        validate_resource_ref_types(&parsed, &schemas, &test_schema_key_fn, &HashSet::new());
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
    });
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::String),
            value: Some(Value::String("dummy".to_string())),
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
        &Value::Int(42),
        &ProviderContext::default(),
    )
    .expect("should error");
    assert!(msg.contains("expected String"), "got: {msg}");
}

#[test]
fn validate_type_expr_struct_error_uses_pascal_case_in_field_type() {
    let mut map = indexmap::IndexMap::new();
    map.insert("count".to_string(), Value::String("x".into()));
    let fields = vec![("count".to_string(), TypeExpr::Int)];
    let msg = validate_type_expr_value(
        &TypeExpr::Struct { fields },
        &Value::Map(map),
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
        &Value::String("10.0.0.0/16".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv4_cidr_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv4_cidr".to_string()),
        &Value::String("not-a-cidr".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_ipv4_address_valid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv4_address".to_string()),
        &Value::String("192.168.1.1".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv4_address_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv4_address".to_string()),
        &Value::String("999.999.999.999".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_ipv6_cidr_valid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_cidr".to_string()),
        &Value::String("2001:db8::/32".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv6_cidr_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_cidr".to_string()),
        &Value::String("not-ipv6-cidr".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_ipv6_address_valid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_address".to_string()),
        &Value::String("2001:db8::1".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_ipv6_address_invalid() {
    let result = validate_type_expr_value(
        &TypeExpr::Simple("ipv6_address".to_string()),
        &Value::String("zzz::zzz".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
}

#[test]
fn validate_type_expr_value_bool_mismatch() {
    let result = validate_type_expr_value(
        &TypeExpr::Bool,
        &Value::String("yes".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool"));
}

#[test]
fn validate_type_expr_value_int_mismatch() {
    let result = validate_type_expr_value(
        &TypeExpr::Int,
        &Value::String("42".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int"));
}

#[test]
fn validate_type_expr_value_float_mismatch() {
    let result = validate_type_expr_value(
        &TypeExpr::Float,
        &Value::String("3.14".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Float"));
}

#[test]
fn validate_type_expr_value_list_of_ipv4_address() {
    let items = vec![
        Value::String("192.168.1.1".to_string()),
        Value::String("999.0.0.1".to_string()),
    ];
    let result = validate_type_expr_value(
        &TypeExpr::List(Box::new(TypeExpr::Simple("ipv4_address".to_string()))),
        &Value::List(items),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("Element 1"));
}

#[test]
fn validate_type_expr_value_string_type_accepts_string() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::String("hello".to_string()),
        &ProviderContext::default(),
    );
    assert!(result.is_none());
}

#[test]
fn validate_type_expr_value_string_got_bool() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Bool(true),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got bool"));
}

#[test]
fn validate_type_expr_value_string_got_int() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Int(42),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got int"));
}

#[test]
fn validate_type_expr_value_string_got_float() {
    let result = validate_type_expr_value(
        &TypeExpr::String,
        &Value::Float(1.5),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got float"));
}

#[test]
fn validate_type_expr_value_bool_got_int() {
    let result =
        validate_type_expr_value(&TypeExpr::Bool, &Value::Int(1), &ProviderContext::default());
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool, got int"));
}

#[test]
fn validate_type_expr_value_int_got_bool() {
    let result = validate_type_expr_value(
        &TypeExpr::Int,
        &Value::Bool(true),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int, got bool"));
}

#[test]
fn validate_type_expr_value_float_got_bool() {
    let result = validate_type_expr_value(
        &TypeExpr::Float,
        &Value::Bool(false),
        &ProviderContext::default(),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Float, got bool"));
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
        &Value::String("vpc-12345678".to_string()),
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
        &Value::Bool(true),
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
    let result =
        validate_type_expr_value(&schema_type, &Value::Int(42), &ProviderContext::default());
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected awscc.ec2.VpcId"));
}

#[test]
fn discard_binding_no_warning() {
    let mut parsed = empty_parsed();

    let caller =
        Resource::with_provider("aws", "sts.caller_identity", "caller_identity").with_binding("_");
    parsed.resources.push(caller); // allow: direct — fixture test inspection

    assert!(check_unused_bindings(&parsed).is_empty());
}

#[test]
fn validate_type_expr_custom_type_rejects_invalid() {
    let config = context_with_iam_policy_arn_validator();

    let result = validate_type_expr_value(
        &TypeExpr::Simple("iam_policy_arn".to_string()),
        &Value::String("aaaa".to_string()),
        &config,
    );
    assert!(result.is_some(), "Expected validation error for 'aaaa'");
    assert!(result.unwrap().contains("invalid IAM policy ARN"));

    let result = validate_type_expr_value(
        &TypeExpr::Simple("iam_policy_arn".to_string()),
        &Value::String("arn:aws:iam::123456789012:policy/MyPolicy".to_string()),
        &config,
    );
    assert!(result.is_none(), "Expected no error for valid ARN");
}

#[test]
fn validate_type_expr_list_custom_type_rejects_invalid() {
    let config = context_with_iam_policy_arn_validator();

    let result = validate_type_expr_value(
        &TypeExpr::List(Box::new(TypeExpr::Simple("iam_policy_arn".to_string()))),
        &Value::List(vec![Value::String("aaaa".to_string())]),
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
    map.insert("name".to_string(), Value::String("x".to_string()));
    map.insert("value".to_string(), Value::Int(1));
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Map(map),
        &ProviderContext::default(),
    );
    assert!(result.is_none(), "got error: {:?}", result);
}

#[test]
fn validate_type_expr_struct_rejects_missing_field() {
    let mut map = IndexMap::new();
    map.insert("name".to_string(), Value::String("x".to_string()));
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Map(map),
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
    map.insert("name".to_string(), Value::String("x".to_string()));
    map.insert("value".to_string(), Value::Int(1));
    map.insert("extra".to_string(), Value::String("y".to_string()));
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Map(map),
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
    map.insert("name".to_string(), Value::String("x".to_string()));
    map.insert("value".to_string(), Value::String("not-an-int".to_string()));
    let result = validate_type_expr_value(
        &struct_type_name_value(),
        &Value::Map(map),
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
        &Value::String("oops".to_string()),
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
    entries.insert("a".to_string(), Value::String("ok".into()));
    entries.insert("z_extra".to_string(), Value::String("x".into()));
    entries.insert("b_extra".to_string(), Value::String("y".into()));
    entries.insert("m_extra".to_string(), Value::String("z".into()));

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

#[test]
fn validate_module_calls_rejects_custom_type() {
    use crate::parser::ArgumentParameter;

    let config = context_with_iam_policy_arn_validator();

    let mut args = HashMap::new();
    args.insert(
        "managed_policy_arns".to_string(),
        Value::List(vec![Value::String("aaaa".to_string())]),
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
    let role = Resource::with_provider("awscc", "iam.role", "github-role")
        .with_binding("role")
        .with_attribute("role_name", Value::String("my-role".to_string()))
        .with_attribute(
            "arn",
            Value::String("arn:aws:iam::123456789012:role/my-role".to_string()),
        );

    let mut role_schema = ResourceSchema::new("iam.role");
    role_schema = role_schema.attribute(AttributeSchema::new("role_name", AttributeType::String));
    role_schema = role_schema.attribute(AttributeSchema::new(
        "arn",
        AttributeType::Custom {
            semantic_name: Some("IamRoleArn".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
            namespace: None,
            to_dsl: None,
        },
    ));

    let mut schemas = HashMap::new();
    schemas.insert("iam.role".to_string(), role_schema);

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

    let result = validate_attribute_param_ref_types(
        &params_mismatch,
        &resources,
        &schemas,
        &|r: &Resource| r.id.resource_type.clone(),
    );
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

    let result =
        validate_attribute_param_ref_types(&params_match, &resources, &schemas, &|r: &Resource| {
            r.id.resource_type.clone()
        });
    assert!(
        result.is_ok(),
        "Should accept IamRoleArn assigned to iam_role_arn"
    );
}

#[test]
fn validate_export_params_rejects_invalid_custom_type() {
    use crate::parser::ExportParameter;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![ExportParameter {
        name: "policy".to_string(),
        type_expr: Some(TypeExpr::Simple("iam_policy_arn".to_string())),
        value: Some(Value::String("not-an-arn".to_string())),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'policy'"), "err={err}");
    assert!(err.contains("invalid IAM policy ARN"), "err={err}");
}

#[test]
fn validate_export_params_rejects_invalid_list_element() {
    use crate::parser::ExportParameter;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![ExportParameter {
        name: "policies".to_string(),
        type_expr: Some(TypeExpr::List(Box::new(TypeExpr::Simple(
            "iam_policy_arn".to_string(),
        )))),
        value: Some(Value::List(vec![
            Value::String("arn:aws:iam::123456789012:policy/valid".to_string()),
            Value::String("garbage".to_string()),
        ])),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'policies'"), "err={err}");
    assert!(err.contains("Element 1"), "err={err}");
}

#[test]
fn validate_export_params_accepts_valid_values() {
    use crate::parser::ExportParameter;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![ExportParameter {
        name: "policy".to_string(),
        type_expr: Some(TypeExpr::Simple("iam_policy_arn".to_string())),
        value: Some(Value::String(
            "arn:aws:iam::123456789012:policy/admin".to_string(),
        )),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_ok());
}

#[test]
fn validate_export_params_skips_no_type_annotation() {
    use crate::parser::ExportParameter;

    let config = ProviderContext::default();
    let exports = vec![ExportParameter {
        name: "raw".to_string(),
        type_expr: None,
        value: Some(Value::String("anything".to_string())),
    }];
    let result = validate_export_params(&exports, &config);
    assert!(result.is_ok());
}

#[test]
fn validate_export_params_rejects_type_mismatch() {
    use crate::parser::ExportParameter;

    let config = ProviderContext::default();
    let exports = vec![ExportParameter {
        name: "flag".to_string(),
        type_expr: Some(TypeExpr::Bool),
        value: Some(Value::String("not-a-bool".to_string())),
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
        semantic_name: Some("KmsKeyArn".to_string()),
        base: Box::new(AttributeType::Custom {
            semantic_name: Some("Arn".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
            namespace: None,
            to_dsl: None,
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
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
        semantic_name: Some("IamRoleArn".to_string()),
        base: Box::new(AttributeType::Custom {
            semantic_name: Some("Arn".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
            namespace: None,
            to_dsl: None,
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
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
        semantic_name: Some("VpcId".to_string()),
        base: Box::new(AttributeType::Custom {
            semantic_name: Some("AwsResourceId".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
            namespace: None,
            to_dsl: None,
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
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
        semantic_name: Some("SubnetId".to_string()),
        base: Box::new(AttributeType::Custom {
            semantic_name: Some("AwsResourceId".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
            namespace: None,
            to_dsl: None,
        }),
        pattern: None,
        length: None,
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
    };
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("vpc_id".to_string()),
        &subnet_id,
    ));
}

#[test]
fn type_compat_exact_match() {
    let arn = AttributeType::Custom {
        semantic_name: Some("Arn".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
    };
    assert!(is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("arn".to_string()),
        &arn,
    ));
}

#[test]
fn type_compat_plain_string_rejected_for_simple() {
    // Simple("aws_account_id") rejects plain String
    assert!(!is_type_expr_compatible_with_schema(
        &TypeExpr::Simple("aws_account_id".to_string()),
        &AttributeType::String,
    ));
}

#[test]
fn validate_export_param_ref_types_map_accepts_compatible_types() {
    use crate::parser::ExportParameter;

    let mut schemas = HashMap::new();
    schemas.insert(
        "organizations.account".to_string(),
        make_schema(
            "organizations.account",
            vec![("account_id", AttributeType::String)],
        ),
    );

    let registry_prod = Resource::with_provider("awscc", "organizations.account", "prod")
        .with_binding("registry_prod")
        .with_attribute("account_id", Value::String("111".to_string()));

    let mut map_value = IndexMap::new();
    map_value.insert(
        "prod".to_string(),
        Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        ),
    );

    let exports = vec![ExportParameter {
        name: "accounts".to_string(),
        // declared as map(string), and values are String-typed — should pass
        type_expr: Some(TypeExpr::Map(Box::new(TypeExpr::String))),
        value: Some(Value::Map(map_value)),
    }];

    let result =
        validate_export_param_ref_types(&exports, &[registry_prod], &schemas, &test_schema_key_fn);
    assert!(
        result.is_ok(),
        "map(String) = {{ prod = registry_prod.account_id (String) }} should pass, got: {:?}",
        result
    );
}

#[test]
fn validate_export_param_ref_types_map_rejects_type_mismatch() {
    use crate::parser::ExportParameter;

    let mut schemas = HashMap::new();
    schemas.insert(
        "organizations.account".to_string(),
        make_schema(
            "organizations.account",
            vec![("account_id", AttributeType::String)],
        ),
    );

    let registry_prod = Resource::with_provider("awscc", "organizations.account", "prod")
        .with_binding("registry_prod")
        .with_attribute("account_id", Value::String("111".to_string()));

    let mut map_value = IndexMap::new();
    map_value.insert(
        "prod".to_string(),
        Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        ),
    );

    let exports = vec![ExportParameter {
        name: "accounts".to_string(),
        // declared as map(bool) — values should be rejected as they are strings
        type_expr: Some(TypeExpr::Map(Box::new(TypeExpr::Bool))),
        value: Some(Value::Map(map_value)),
    }];

    let result =
        validate_export_param_ref_types(&exports, &[registry_prod], &schemas, &test_schema_key_fn);
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

    let mut schemas = HashMap::new();
    schemas.insert("ec2.Vpc".to_string(), schema);

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc");

    let mut known = HashSet::new();
    known.insert("awscc".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    let err = validate_resources(&parsed, &schemas, &test_schema_key_fn, &known).unwrap_err();
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

    let mut schemas = HashMap::new();
    schemas.insert(
        "r.mode_holder".to_string(),
        make_schema(
            "r.mode_holder",
            vec![(
                "mode",
                AttributeType::StringEnum {
                    name: "Mode".to_string(),
                    values: vec!["on".to_string(), "off".to_string()],
                    namespace: None,
                    to_dsl: None,
                },
            )],
        ),
    );

    let mut known = HashSet::new();
    known.insert("test".to_string());

    let result = validate_resources(&parsed, &schemas, &test_schema_key_fn, &known);
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
fn mode_schema() -> HashMap<String, ResourceSchema> {
    let mut schemas = HashMap::new();
    schemas.insert(
        "r.mode_holder".to_string(),
        make_schema(
            "r.mode_holder",
            vec![(
                "mode",
                AttributeType::StringEnum {
                    name: "Mode".to_string(),
                    values: vec!["fast".to_string(), "slow".to_string()],
                    namespace: Some("test.r".to_string()),
                    to_dsl: None,
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
    let err = validate_resources(&parsed, &mode_schema(), &test_schema_key_fn, &mode_known())
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
    let err = validate_resources(&parsed, &mode_schema(), &test_schema_key_fn, &mode_known())
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
        validate_resources(&parsed, &mode_schema(), &test_schema_key_fn, &mode_known()).is_ok(),
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
        validate_resources(&parsed, &mode_schema(), &test_schema_key_fn, &mode_known()).is_ok(),
        "fully-qualified identifier must pass"
    );
}
