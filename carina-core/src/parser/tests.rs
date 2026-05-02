use super::*;
use crate::resource::{InterpolationPart, Resource, Value};
use indexmap::IndexMap;
use std::collections::HashMap;

#[test]
fn parse_and_resolve_returns_value_only_no_closure() {
    // Issue #2230 acceptance criterion 3: `parse_and_resolve` must
    // never expose a closure to its caller. Type-system enforcement
    // makes the literal claim trivially true (`Value::Closure` does
    // not exist), so this test doubles as a smoke check that
    // legitimate partial-application expressions (data-last pipes
    // + builtin chaining) still parse and produce a `Value` tree
    // that no consumer needs to inspect for a closure case.
    let input = r#"
        let xs = ["a", "b", "c"]
        let joined = xs |> join("-")
    "#;
    let parsed = parse_and_resolve(input).expect("parse_and_resolve should succeed");
    let joined = parsed
        .variables
        .get("joined")
        .expect("joined binding present");
    // No `Closure` arm exists on `Value`, so the only way this
    // could fail is if the call survived as a `FunctionCall` —
    // also a valid `Value`, never a closure. The point of the
    // test is that the type contract holds: whatever shape this
    // is, downstream code does not have to consider closures.
    match joined {
        Value::String(_) | Value::FunctionCall { .. } => {}
        other => panic!("unexpected variant for `joined`: {other:?}"),
    }
}

#[test]
fn unfinished_closure_in_let_binding_is_dropped() {
    // Issue #2230 acceptance criterion 2: a `let` binding holding
    // an unfinished partial application must not surface a closure
    // to the caller. The evaluator-internal `EvalValue::Closure`
    // is dropped at the lowering boundary; the binding name simply
    // does not appear in `ParsedFile.variables`.
    let input = r#"let f = join("-")"#;
    let parsed = parse_and_resolve(input).expect("partial application in let binding should parse");
    assert!(
        parsed.variables.get("f").is_none(),
        "closure binding must not survive into ParsedFile.variables"
    );
}

#[test]
fn iter_all_resources_yields_direct_then_deferred() {
    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        test.r.res {
            name = "direct"
        }
        for _, id in orgs.accounts {
            test.r.res {
                name = id
            }
        }
    "#;
    let parsed = parse(src, &ProviderContext::default()).unwrap();

    let items: Vec<_> = parsed.iter_all_resources().collect();
    assert_eq!(items.len(), 2, "expected one direct + one deferred");

    assert!(matches!(items[0].0, ResourceContext::Direct));
    assert_eq!(
        items[0].1.get_attr("name"),
        Some(&Value::String("direct".to_string()))
    );

    assert!(matches!(items[1].0, ResourceContext::Deferred(_)));
}

#[test]
fn parse_provider_block() {
    let input = r#"
        provider aws {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].name, "aws");
}

#[test]
fn parse_resource_with_namespaced_type() {
    let input = r#"
        let my_bucket = aws.s3_bucket {
            name = "my-bucket"
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert_eq!(resource.id.resource_type, "s3_bucket");
    assert_eq!(resource.id.name_str(), "my_bucket"); // binding name becomes the resource ID
    assert_eq!(
        resource.get_attr("name"),
        Some(&Value::String("my-bucket".to_string()))
    );
    assert_eq!(
        resource.get_attr("region"),
        Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
    );
}

#[test]
fn parse_multiple_resources() {
    let input = r#"
        let logs = aws.s3_bucket {
            name = "app-logs"
        }

        let data = aws.s3_bucket {
            name = "app-data"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);
    assert_eq!(result.resources[0].id.name_str(), "logs"); // binding name becomes the resource ID
    assert_eq!(result.resources[1].id.name_str(), "data");
}

#[test]
fn parse_variable_and_resource() {
    let input = r#"
        let default_region = aws.Region.ap_northeast_1

        let my_bucket = aws.s3_bucket {
            name = "my-bucket"
            region = default_region
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("region"),
        Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
    );
}

#[test]
fn parse_full_example() {
    let input = r#"
        # Provider configuration
        provider aws {
            region = aws.Region.ap_northeast_1
        }

        # Variables
        let versioning = true
        let retention_days = 90

        # Resources
        let app_logs = aws.s3_bucket {
            name = "my-app-logs"
            versioning = versioning
            expiration_days = retention_days
        }

        let app_data = aws.s3_bucket {
            name = "my-app-data"
            versioning = versioning
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.resources.len(), 2);
    assert_eq!(
        result.resources[0].get_attr("versioning"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        result.resources[0].get_attr("expiration_days"),
        Some(&Value::Int(90))
    );
}

#[test]
fn function_call_is_parsed() {
    let input = r#"
        let my_bucket = aws.s3_bucket {
            name = env("SOME_VAR")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::FunctionCall {
            name: "env".to_string(),
            args: vec![Value::String("SOME_VAR".to_string())],
        })
    );
}

#[test]
fn parse_gcp_resource() {
    let input = r#"
        let my_bucket = gcp.storage.bucket {
            name = "my-gcp-bucket"
            location = gcp.Location.asia_northeast1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id.resource_type, "storage.bucket");
    assert_eq!(result.resources[0].id.provider, "gcp");
    // _provider attribute should NOT be set (provider identity is in ResourceId)
    assert!(!result.resources[0].attributes.contains_key("_provider"));
}

#[test]
fn parse_anonymous_resource() {
    let input = r#"
        aws.s3_bucket {
            name = "my-anonymous-bucket"
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert_eq!(resource.id.resource_type, "s3_bucket");
    assert_eq!(resource.id.name_str(), ""); // anonymous resources get empty name (computed later)
}

#[test]
fn parse_mixed_resources() {
    let input = r#"
        # Anonymous resource
        aws.s3_bucket {
            name = "anonymous-bucket"
        }

        # Named resource
        let named = aws.s3_bucket {
            name = "named-bucket"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);
    assert_eq!(result.resources[0].id.name_str(), ""); // anonymous gets empty name
    assert_eq!(result.resources[1].id.name_str(), "named"); // binding name becomes the resource ID
}

#[test]
fn parse_anonymous_resource_without_name_succeeds() {
    let input = r#"
        aws.s3_bucket {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok());
    let parsed = result.unwrap();
    assert_eq!(parsed.resources[0].id.name_str(), ""); // empty name, computed later
}

#[test]
fn parse_resource_reference() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = "my-bucket"
            region = aws.Region.ap_northeast_1
        }

        let policy = aws.s3_bucket_policy {
            name = "my-policy"
            bucket = bucket.name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    // Before resolution, the attribute should be a ResourceRef
    let policy = &result.resources[1];
    assert_eq!(
        policy.get_attr("bucket"),
        Some(&Value::resource_ref(
            "bucket".to_string(),
            "name".to_string(),
            vec![]
        ))
    );
}

#[test]
fn parse_and_resolve_resource_reference() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = "my-bucket"
            region = aws.Region.ap_northeast_1
        }

        let policy = aws.s3_bucket_policy {
            name = "my-policy"
            bucket = bucket.name
            bucket_region = bucket.region
        }
    "#;

    let result = parse_and_resolve(input).unwrap();
    assert_eq!(result.resources.len(), 2);

    // After resolution, the attribute should be the actual value
    let policy = &result.resources[1];
    assert_eq!(
        policy.get_attr("bucket"),
        Some(&Value::String("my-bucket".to_string()))
    );
    assert_eq!(
        policy.get_attr("bucket_region"),
        Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
    );
}

#[test]
fn parse_undefined_two_part_identifier_becomes_string() {
    // When a 2-part identifier references an unknown binding,
    // it becomes a String (e.g., "nonexistent.name") for later schema validation
    let input = r#"
        let policy = aws.s3_bucket_policy {
            name = "my-policy"
            bucket = nonexistent.name
        }
    "#;

    // Parsing succeeds - unknown identifiers become String
    let result = parse_and_resolve(input);
    assert!(result.is_ok());
    let parsed = result.unwrap();
    assert_eq!(
        parsed.resources[0].get_attr("bucket"),
        Some(&Value::String("nonexistent.name".to_string()))
    );
}

#[test]
fn parse_bare_identifier_becomes_string() {
    // When a bare identifier is not a known variable or binding,
    // it becomes a String for later schema validation (enum resolution)
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            instance_tenancy = dedicated
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("instance_tenancy"),
        Some(&Value::String("dedicated".to_string()))
    );
}

#[test]
fn resource_reference_preserves_namespaced_id() {
    // Ensure that aws.Region.ap_northeast_1 is NOT treated as a resource reference
    let input = r#"
        let bucket = aws.s3_bucket {
            name = "my-bucket"
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("region"),
        Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
    );
}

#[test]
fn namespaced_id_with_digit_segment() {
    // Enum values containing dots (e.g., "ipsec.1") should be parsed
    // as part of a namespaced_id when written as an identifier
    let input = r#"
        let gw = awscc.ec2.vpn_gateway {
            type = awscc.ec2.vpn_gateway.Type.ipsec.1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("type"),
        Some(&Value::String(
            "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
        ))
    );
}

#[test]
fn parse_nested_blocks_terraform_style() {
    let input = r#"
        let web_sg = aws.security_group {
            name        = "web-sg"
            region      = aws.Region.ap_northeast_1
            vpc         = "my-vpc"
            description = "Web server security group"

            ingress {
                protocol  = "tcp"
                from_port = 80
                to_port   = 80
                cidr      = "0.0.0.0/0"
            }

            ingress {
                protocol  = "tcp"
                from_port = 443
                to_port   = 443
                cidr      = "0.0.0.0/0"
            }

            egress {
                protocol  = "-1"
                from_port = 0
                to_port   = 0
                cidr      = "0.0.0.0/0"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let sg = &result.resources[0];
    assert_eq!(sg.id.resource_type, "security_group");

    // Check ingress is a list with 2 items
    let ingress = sg.get_attr("ingress").unwrap();
    if let Value::List(items) = ingress {
        assert_eq!(items.len(), 2);

        // Check first ingress rule
        if let Value::Map(rule) = &items[0] {
            assert_eq!(
                rule.get("protocol"),
                Some(&Value::String("tcp".to_string()))
            );
            assert_eq!(rule.get("from_port"), Some(&Value::Int(80)));
        } else {
            panic!("Expected map for ingress rule");
        }
    } else {
        panic!("Expected list for ingress");
    }

    // Check egress is a list with 1 item
    let egress = sg.get_attr("egress").unwrap();
    if let Value::List(items) = egress {
        assert_eq!(items.len(), 1);
    } else {
        panic!("Expected list for egress");
    }
}

#[test]
fn parse_list_syntax() {
    let input = r#"
        let rt = aws.route_table {
            name   = "public-rt"
            region = aws.Region.ap_northeast_1
            vpc    = "my-vpc"
            routes = [
                { destination = "0.0.0.0/0", gateway = "my-igw" },
                { destination = "10.0.0.0/8", gateway = "local" }
            ]
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let rt = &result.resources[0];
    let routes = rt.get_attr("routes").unwrap();
    if let Value::List(items) = routes {
        assert_eq!(items.len(), 2);

        if let Value::Map(route) = &items[0] {
            assert_eq!(
                route.get("destination"),
                Some(&Value::String("0.0.0.0/0".to_string()))
            );
            assert_eq!(
                route.get("gateway"),
                Some(&Value::String("my-igw".to_string()))
            );
        } else {
            panic!("Expected map for route");
        }
    } else {
        panic!("Expected list for routes");
    }
}

#[test]
fn parse_directory_module() {
    let input = r#"
        arguments {
            vpc_id: String
            enable_https: Bool = true
        }

        attributes {
            sg_id: String = web_sg.id
        }

        let web_sg = aws.security_group {
            name   = "web-sg"
            vpc_id = vpc_id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    // Check arguments
    assert_eq!(result.arguments.len(), 2);
    assert_eq!(result.arguments[0].name, "vpc_id");
    assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
    assert!(result.arguments[0].default.is_none());

    assert_eq!(result.arguments[1].name, "enable_https");
    assert_eq!(result.arguments[1].type_expr, TypeExpr::Bool);
    assert_eq!(result.arguments[1].default, Some(Value::Bool(true)));

    // Check attribute params
    assert_eq!(result.attribute_params.len(), 1);
    assert_eq!(result.attribute_params[0].name, "sg_id");
    assert_eq!(result.attribute_params[0].type_expr, Some(TypeExpr::String));

    // Check resource has argument reference (lexically scoped)
    assert_eq!(result.resources.len(), 1);
    let sg = &result.resources[0];
    assert_eq!(
        sg.get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "vpc_id".to_string(),
            String::new(),
            vec![]
        ))
    );
}

#[test]
fn parse_use_expression() {
    let input = r#"
        let web_tier = use { source = "./modules/web_tier" }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.uses.len(), 1);
    assert_eq!(result.uses[0].path, "./modules/web_tier");
    assert_eq!(result.uses[0].alias, "web_tier");
}

#[test]
fn parse_use_expression_requires_source() {
    let input = r#"
        let web_tier = use { }
    "#;

    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("source"),
        "error should mention missing source, got: {msg}"
    );
}

#[test]
fn parse_use_expression_rejects_unknown_attribute() {
    let input = r#"
        let web_tier = use { source = "./x", bogus = "y" }
    "#;

    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("bogus"),
        "error should mention unexpected attribute, got: {msg}"
    );
}

// The `use` expression is only valid as a top-level `let` binding RHS.
// The grammar previously accepted it in any primary-value position, which
// produced silent evaluator failures (issue #2233). These tests pin the
// grammar boundary: any non-let-RHS position must be a parse error.

#[test]
fn parse_use_expression_rejected_as_module_call_argument() {
    let input = r#"
        some_module {
          network = use { source = "./modules/network" }
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "use expression as module-call argument must be rejected, got: {result:?}"
    );
}

#[test]
fn parse_use_expression_rejected_in_list() {
    let input = r#"
        let mods = [use { source = "./modules/a" }]
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "use expression inside a list must be rejected, got: {result:?}"
    );
}

#[test]
fn parse_use_expression_rejected_in_if_branch() {
    let input = r#"
        let net = if true { use { source = "./a" } } else { use { source = "./b" } }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "use expression inside an if branch must be rejected, got: {result:?}"
    );
}

#[test]
fn parse_use_expression_rejected_in_local_let() {
    // `local_binding` (block-scoped `let`) goes through `parse_expression`,
    // which has no `use_expr` handling. Must be a parse error, not silent failure.
    let input = r#"
        aws.s3.bucket {
          name = "my-bucket"
          let mod_x = use { source = "./modules/x" }
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "use expression inside a local let binding must be rejected, got: {result:?}"
    );
}

#[test]
fn parse_generic_type_expressions() {
    let input = r#"
        arguments {
            ports: list(Int)
            tags: map(String)
            cidrs: list(String)
        }

        attributes {
            result: list(String) = items.ids
        }

        let items = aws.item {
            name = "test"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::List(Box::new(TypeExpr::Int))
    );
    assert_eq!(
        result.arguments[1].type_expr,
        TypeExpr::Map(Box::new(TypeExpr::String))
    );
    assert_eq!(
        result.arguments[2].type_expr,
        TypeExpr::List(Box::new(TypeExpr::String))
    );
    assert_eq!(
        result.attribute_params[0].type_expr,
        Some(TypeExpr::List(Box::new(TypeExpr::String)))
    );
    assert!(result.attribute_params[0].value.is_some());
}

#[test]
fn parse_ref_type_expression() {
    let input = r#"
        arguments {
            vpc: aws.vpc
            enable_https: Bool = true
        }

        attributes {
            security_group_id: aws.security_group = web_sg.id
        }

        let web_sg = aws.security_group {
            name   = "web-sg"
            vpc_id = vpc
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    // Check ref type argument
    assert_eq!(result.arguments[0].name, "vpc");
    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::Ref(ResourceTypePath::new("aws", "vpc"))
    );
    assert!(result.arguments[0].default.is_none());

    // Check ref type attribute param
    assert_eq!(result.attribute_params[0].name, "security_group_id");
    assert_eq!(
        result.attribute_params[0].type_expr,
        Some(TypeExpr::Ref(ResourceTypePath::new(
            "aws",
            "security_group"
        )))
    );
}

#[test]
fn parse_ref_type_with_nested_resource_type() {
    let input = r#"
        arguments {
            sg: aws.security_group
            rule: aws.security_group.ingress_rule
        }

        attributes {
            out: String = sg.name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    // Single-level resource type
    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::Ref(ResourceTypePath::new("aws", "security_group"))
    );

    // Nested resource type (security_group.ingress_rule)
    assert_eq!(
        result.arguments[1].type_expr,
        TypeExpr::Ref(ResourceTypePath::new("aws", "security_group.ingress_rule"))
    );
}

#[test]
fn parse_struct_type_expression() {
    let input = r#"
        exports {
            accounts: struct {
                registry_prod: AwsAccountId,
                registry_dev: AwsAccountId,
            } = {
                registry_prod = "111111111111"
                registry_dev  = "222222222222"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.export_params.len(), 1);
    let ep = &result.export_params[0];
    assert_eq!(ep.name, "accounts");
    let expected = TypeExpr::Struct {
        fields: vec![
            (
                "registry_prod".to_string(),
                TypeExpr::Simple("aws_account_id".to_string()),
            ),
            (
                "registry_dev".to_string(),
                TypeExpr::Simple("aws_account_id".to_string()),
            ),
        ],
    };
    assert_eq!(ep.type_expr, Some(expected));
}

#[test]
fn parse_struct_type_nested_in_list_and_map() {
    let input = r#"
        arguments {
            items: list(struct { name: String, value: Int })
            registry: map(struct { arn: String, id: String })
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::List(Box::new(TypeExpr::Struct {
            fields: vec![
                ("name".to_string(), TypeExpr::String),
                ("value".to_string(), TypeExpr::Int),
            ],
        }))
    );
    assert_eq!(
        result.arguments[1].type_expr,
        TypeExpr::Map(Box::new(TypeExpr::Struct {
            fields: vec![
                ("arn".to_string(), TypeExpr::String),
                ("id".to_string(), TypeExpr::String),
            ],
        }))
    );
}

#[test]
fn parse_struct_type_rejects_duplicate_field_name() {
    let input = r#"
        exports {
            x: struct { a: String, a: Int } = { a = "hi" }
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("duplicate field name 'a'"),
        "expected duplicate-name error, got: {msg}"
    );
}

#[test]
fn struct_type_expr_display_renders_with_braces() {
    let t = TypeExpr::Struct {
        fields: vec![
            ("name".to_string(), TypeExpr::String),
            ("value".to_string(), TypeExpr::Int),
        ],
    };
    assert_eq!(t.to_string(), "struct { name: String, value: Int }");

    let empty = TypeExpr::Struct { fields: vec![] };
    assert_eq!(empty.to_string(), "struct {}");
}

#[test]
fn struct_type_expr_roundtrips_through_serde_json() {
    let t = TypeExpr::Struct {
        fields: vec![
            ("name".to_string(), TypeExpr::String),
            ("value".to_string(), TypeExpr::Int),
        ],
    };
    let json = serde_json::to_string(&t).unwrap();
    let back: TypeExpr = serde_json::from_str(&json).unwrap();
    assert_eq!(t, back);
}

#[test]
fn parse_attributes_without_type_annotation() {
    let input = r#"
        attributes {
            security_group = sg.id
        }

        let sg = aws.security_group {
            name = "web-sg"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    assert_eq!(result.attribute_params.len(), 1);
    assert_eq!(result.attribute_params[0].name, "security_group");
    assert_eq!(result.attribute_params[0].type_expr, None);
    assert!(result.attribute_params[0].value.is_some());
}

#[test]
fn parse_attributes_mixed_typed_and_untyped() {
    let input = r#"
        attributes {
            vpc_id: awscc.ec2.VpcId = vpc.vpc_id
            security_group = sg.id
            subnet_ids: list(String) = subnets.ids
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }

        let sg = aws.security_group {
            name = "web-sg"
        }

        let subnets = aws.subnet {
            vpc_id = vpc.vpc_id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    assert_eq!(result.attribute_params.len(), 3);

    // Explicit type
    assert_eq!(result.attribute_params[0].name, "vpc_id");
    assert!(result.attribute_params[0].type_expr.is_some());
    assert!(result.attribute_params[0].value.is_some());

    // No type annotation
    assert_eq!(result.attribute_params[1].name, "security_group");
    assert_eq!(result.attribute_params[1].type_expr, None);
    assert!(result.attribute_params[1].value.is_some());

    // Explicit type
    assert_eq!(result.attribute_params[2].name, "subnet_ids");
    assert_eq!(
        result.attribute_params[2].type_expr,
        Some(TypeExpr::List(Box::new(TypeExpr::String)))
    );
    assert!(result.attribute_params[2].value.is_some());
}

#[test]
fn resource_type_path_parse() {
    // Simple resource type
    let path = ResourceTypePath::parse("aws.vpc").unwrap();
    assert_eq!(path.provider, "aws");
    assert_eq!(path.resource_type, "vpc");

    // Nested resource type
    let path2 = ResourceTypePath::parse("aws.security_group.ingress_rule").unwrap();
    assert_eq!(path2.provider, "aws");
    assert_eq!(path2.resource_type, "security_group.ingress_rule");

    // Invalid (single component)
    assert!(ResourceTypePath::parse("vpc").is_none());
}

#[test]
fn resource_type_path_display() {
    let path = ResourceTypePath::new("aws", "vpc");
    assert_eq!(path.to_string(), "aws.vpc");

    let path2 = ResourceTypePath::new("aws", "security_group.ingress_rule");
    assert_eq!(path2.to_string(), "aws.security_group.ingress_rule");
}

#[test]
fn type_expr_display_with_ref() {
    assert_eq!(TypeExpr::String.to_string(), "String");
    assert_eq!(TypeExpr::Bool.to_string(), "Bool");
    assert_eq!(TypeExpr::Int.to_string(), "Int");
    assert_eq!(
        TypeExpr::List(Box::new(TypeExpr::String)).to_string(),
        "list(String)"
    );
    assert_eq!(
        TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")).to_string(),
        "aws.vpc"
    );
}

#[test]
fn parse_float_literal() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = "test"
            weight = 2.5
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("weight"),
        Some(&Value::Float(2.5))
    );
}

#[test]
fn parse_negative_float_literal() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = "test"
            offset = -0.5
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("offset"),
        Some(&Value::Float(-0.5))
    );
}

#[test]
fn type_expr_display_float() {
    assert_eq!(TypeExpr::Float.to_string(), "Float");
}

#[test]
fn type_expr_display_primitives_are_pascal_case() {
    assert_eq!(TypeExpr::String.to_string(), "String");
    assert_eq!(TypeExpr::Int.to_string(), "Int");
    assert_eq!(TypeExpr::Bool.to_string(), "Bool");
    assert_eq!(TypeExpr::Float.to_string(), "Float");
    assert_eq!(
        TypeExpr::List(Box::new(TypeExpr::Int)).to_string(),
        "list(Int)"
    );
    assert_eq!(
        TypeExpr::Map(Box::new(TypeExpr::String)).to_string(),
        "map(String)"
    );
}

#[test]
fn parse_backend_block() {
    let input = r#"
        backend s3 {
            bucket      = "my-carina-state"
            key         = "infra/prod/carina.crnstate"
            region      = aws.Region.ap_northeast_1
            encrypt     = true
            auto_create = true
        }

        provider aws {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    // Check backend
    assert!(result.backend.is_some());
    let backend = result.backend.unwrap();
    assert_eq!(backend.backend_type, "s3");
    assert_eq!(
        backend.attributes.get("bucket"),
        Some(&Value::String("my-carina-state".to_string()))
    );
    assert_eq!(
        backend.attributes.get("key"),
        Some(&Value::String("infra/prod/carina.crnstate".to_string()))
    );
    assert_eq!(
        backend.attributes.get("region"),
        Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
    );
    assert_eq!(backend.attributes.get("encrypt"), Some(&Value::Bool(true)));
    assert_eq!(
        backend.attributes.get("auto_create"),
        Some(&Value::Bool(true))
    );

    // Check provider
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].name, "aws");
}

#[test]
fn parse_backend_block_with_resources() {
    let input = r#"
        backend s3 {
            bucket = "my-state"
            key    = "prod/carina.state"
            region = aws.Region.ap_northeast_1
        }

        provider aws {
            region = aws.Region.ap_northeast_1
        }

        aws.s3_bucket {
            name       = "my-state"
            versioning = "Enabled"
        }

        aws.ec2.Vpc {
            name       = "main-vpc"
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    assert!(result.backend.is_some());
    let backend = result.backend.unwrap();
    assert_eq!(backend.backend_type, "s3");
    assert_eq!(
        backend.attributes.get("bucket"),
        Some(&Value::String("my-state".to_string()))
    );

    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.resources.len(), 2);
}

#[test]
fn parse_read_resource_expr() {
    let input = r#"
        let existing = read aws.s3_bucket {
            name = "my-existing-bucket"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert_eq!(resource.id.resource_type, "s3_bucket");
    assert_eq!(resource.id.name_str(), "existing"); // binding name becomes the resource ID
    assert!(resource.is_data_source());
}

#[test]
fn parse_read_resource_does_not_inject_data_source_attribute() {
    // Regression test for #2224: `kind == DataSource` is the only
    // record that a `read` block produces a data source — there must
    // be no `_data_source` key shadowing it in the attribute map.
    let input = r#"
        let existing = read aws.s3_bucket {
            name = "my-bucket"
            region = "us-east-1"
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let resource = &result.resources[0];
    assert!(resource.is_data_source());
    assert!(resource.attributes.contains_key("name"));
    assert!(resource.attributes.contains_key("region"));
    assert!(!resource.attributes.contains_key("_data_source"));
}

#[test]
fn parse_read_resource_without_name_uses_binding() {
    let input = r#"
        let existing = read aws.s3_bucket {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok());
    let parsed = result.unwrap();
    assert_eq!(parsed.resources[0].id.name_str(), "existing"); // binding name
}

#[test]
fn parse_read_with_regular_resources() {
    let input = r#"
        # Read existing bucket (data source)
        let existing_bucket = read aws.s3_bucket {
            name = "existing-bucket"
        }

        # Create new bucket that depends on reading the existing one
        let new_bucket = aws.s3_bucket {
            name = "new-bucket"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    // First resource is read-only (data source)
    assert!(result.resources[0].is_data_source());
    assert_eq!(result.resources[0].id.name_str(), "existing_bucket"); // binding name

    // Second resource is a regular resource
    assert!(!result.resources[1].is_data_source());
    assert_eq!(result.resources[1].id.name_str(), "new_bucket"); // binding name
}

#[test]
fn parse_lifecycle_force_delete() {
    let input = r#"
        let bucket = awscc.s3_bucket {
            bucket_name = "my-bucket"
            lifecycle {
                force_delete = true
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert_eq!(resource.id.resource_type, "s3_bucket");
    assert!(resource.lifecycle.force_delete);
    // lifecycle should NOT appear in attributes
    assert!(!resource.attributes.contains_key("lifecycle"));
}

#[test]
fn parse_lifecycle_default_when_absent() {
    let input = r#"
        let bucket = awscc.s3_bucket {
            bucket_name = "my-bucket"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert!(!result.resources[0].lifecycle.force_delete);
    assert!(!result.resources[0].lifecycle.prevent_destroy);
}

#[test]
fn parse_lifecycle_anonymous_resource() {
    let input = r#"
        awscc.s3_bucket {
            bucket_name = "my-bucket"
            lifecycle {
                force_delete = true
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert!(result.resources[0].lifecycle.force_delete);
    assert!(!result.resources[0].attributes.contains_key("lifecycle"));
}

/// Regression test for issue #146: anonymous AWSCC resources should not have
/// a spurious "name" attribute injected into the attributes map.
#[test]
fn anonymous_resource_no_spurious_name_attribute() {
    let input = r#"
        awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert_eq!(resource.id.name_str(), ""); // anonymous → empty name
    // "name" must NOT appear in attributes unless the user explicitly wrote it
    assert!(
        !resource.attributes.contains_key("name"),
        "Anonymous AWSCC resource should not have 'name' in attributes, but found: {:?}",
        resource.get_attr("name")
    );
}

/// Regression test for issue #146: let-bound AWSCC resources should not have
/// a spurious "name" attribute injected by the parser.
#[test]
fn let_bound_resource_no_spurious_name_attribute() {
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert_eq!(resource.id.name_str(), "vpc"); // binding name → resource name
    // "name" must NOT appear in attributes (it's only the id.name, not an attribute)
    assert!(
        !resource.attributes.contains_key("name"),
        "Let-bound AWSCC resource should not have 'name' in attributes, but found: {:?}",
        resource.get_attr("name")
    );
}

#[test]
fn parse_lifecycle_create_before_destroy() {
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
            lifecycle {
                create_before_destroy = true
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert!(resource.lifecycle.create_before_destroy);
    assert!(!resource.lifecycle.force_delete);
    assert!(!resource.attributes.contains_key("lifecycle"));
}

#[test]
fn parse_lifecycle_both_force_delete_and_create_before_destroy() {
    let input = r#"
        let bucket = awscc.s3_bucket {
            bucket_name = "my-bucket"
            lifecycle {
                force_delete = true
                create_before_destroy = true
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let resource = &result.resources[0];
    assert!(resource.lifecycle.force_delete);
    assert!(resource.lifecycle.create_before_destroy);
    assert!(!resource.attributes.contains_key("lifecycle"));
}

#[test]
fn parse_block_syntax_inside_map() {
    let input = r#"
        let role = awscc.iam.role {
            assume_role_policy_document = {
                version = "2012-10-17"
                statement {
                    effect    = "Allow"
                    principal = { service = "lambda.amazonaws.com" }
                    action    = "sts:AssumeRole"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);

    let role = &result.resources[0];
    let doc = role.get_attr("assume_role_policy_document").unwrap();
    if let Value::Map(map) = doc {
        assert_eq!(
            map.get("version"),
            Some(&Value::String("2012-10-17".to_string()))
        );
        // statement block becomes a list with one element
        let statement = map.get("statement").unwrap();
        if let Value::List(stmts) = statement {
            assert_eq!(stmts.len(), 1);
            if let Value::Map(stmt) = &stmts[0] {
                assert_eq!(
                    stmt.get("effect"),
                    Some(&Value::String("Allow".to_string()))
                );
                assert_eq!(
                    stmt.get("action"),
                    Some(&Value::String("sts:AssumeRole".to_string()))
                );
            } else {
                panic!("Expected map for statement");
            }
        } else {
            panic!("Expected list for statement");
        }
    } else {
        panic!("Expected map for assume_role_policy_document");
    }
}

#[test]
fn parse_multiple_blocks_inside_map() {
    let input = r#"
        let role = awscc.iam.role {
            policy_document = {
                version = "2012-10-17"
                statement {
                    effect = "Allow"
                    action = "s3:GetObject"
                }
                statement {
                    effect = "Deny"
                    action = "s3:DeleteObject"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let role = &result.resources[0];
    let doc = role.get_attr("policy_document").unwrap();
    if let Value::Map(map) = doc {
        let statement = map.get("statement").unwrap();
        if let Value::List(stmts) = statement {
            assert_eq!(stmts.len(), 2);
        } else {
            panic!("Expected list for statement");
        }
    } else {
        panic!("Expected map for policy_document");
    }
}

#[test]
fn parse_list_syntax_inside_map_still_works() {
    // Backward compatibility: list literal syntax still works
    let input = r#"
        let role = awscc.iam.role {
            assume_role_policy_document = {
                version = "2012-10-17"
                statement = [
                    {
                        effect    = "Allow"
                        principal = { service = "lambda.amazonaws.com" }
                        action    = "sts:AssumeRole"
                    }
                ]
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let role = &result.resources[0];
    let doc = role.get_attr("assume_role_policy_document").unwrap();
    if let Value::Map(map) = doc {
        let statement = map.get("statement").unwrap();
        if let Value::List(stmts) = statement {
            assert_eq!(stmts.len(), 1);
        } else {
            panic!("Expected list for statement");
        }
    } else {
        panic!("Expected map for assume_role_policy_document");
    }
}

#[test]
fn parse_deeply_nested_blocks() {
    // Test nested blocks at depth 2: resource { outer { inner { ... } } }
    let input = r#"
        let r = aws.test.resource {
            outer {
                inner {
                    leaf = "value"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let r = &result.resources[0];

    let outer = r.get_attr("outer").unwrap();
    if let Value::List(outer_items) = outer {
        assert_eq!(outer_items.len(), 1);
        if let Value::Map(outer_map) = &outer_items[0] {
            let inner = outer_map.get("inner").unwrap();
            if let Value::List(inner_items) = inner {
                assert_eq!(inner_items.len(), 1);
                if let Value::Map(inner_map) = &inner_items[0] {
                    assert_eq!(
                        inner_map.get("leaf"),
                        Some(&Value::String("value".to_string()))
                    );
                } else {
                    panic!("Expected map for inner block");
                }
            } else {
                panic!("Expected list for inner");
            }
        } else {
            panic!("Expected map for outer block");
        }
    } else {
        panic!("Expected list for outer");
    }
}

#[test]
fn parse_nested_block_in_map() {
    // Test nested block inside map value: attr = { block { ... } }
    let input = r#"
        let role = aws.iam.Role {
            policy_document = {
                statement {
                    effect = "Allow"
                    action = "s3:GetObject"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let role = &result.resources[0];

    let doc = role.get_attr("policy_document").unwrap();
    if let Value::Map(map) = doc {
        let statement = map.get("statement").unwrap();
        if let Value::List(items) = statement {
            assert_eq!(items.len(), 1);
            if let Value::Map(s) = &items[0] {
                assert_eq!(s.get("effect"), Some(&Value::String("Allow".to_string())));
            } else {
                panic!("Expected map for statement");
            }
        } else {
            panic!("Expected list for statement");
        }
    } else {
        panic!("Expected map for policy_document");
    }
}

#[test]
fn test_find_resource_by_attr() {
    let input = r#"
        aws.s3.Bucket {
            bucket = "my-bucket"
        }
        aws.s3.Bucket {
            bucket = "other-bucket"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();

    assert!(
        parsed
            .find_resource_by_attr("s3.Bucket", "bucket", "my-bucket")
            .is_some()
    );
    assert!(
        parsed
            .find_resource_by_attr("s3.Bucket", "bucket", "other-bucket")
            .is_some()
    );
    assert!(
        parsed
            .find_resource_by_attr("s3.Bucket", "bucket", "no-such")
            .is_none()
    );
    assert!(
        parsed
            .find_resource_by_attr("ec2.Vpc", "bucket", "my-bucket")
            .is_none()
    );
}

#[test]
fn parse_integer_overflow_returns_error() {
    // i64::MAX is 9223372036854775807; one more should fail
    let input = r#"
provider aws {
region = aws.Region.ap_northeast_1
}

aws.s3.Bucket {
name = "test"
count = 99999999999999999999
}
"#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("integer literal out of range"),
        "expected 'integer literal out of range' error, got: {err}"
    );
}

#[test]
fn pipe_operator_desugars_to_function_call() {
    let input = r#"
        let x = "hello" |> upper()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    // "hello" |> upper() desugars to upper("hello")
    assert_eq!(
        result.variables.get("x"),
        Some(&Value::FunctionCall {
            name: "upper".to_string(),
            args: vec![Value::String("hello".to_string())],
        })
    );
}

#[test]
fn pipe_operator_in_attribute_desugars() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = "test" |> lower()
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::FunctionCall {
            name: "lower".to_string(),
            args: vec![Value::String("test".to_string())],
        })
    );
}

#[test]
fn join_function_call_parsed() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = join("-", ["a", "b", "c"])
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    // At parse time, function calls remain as FunctionCall values
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                    Value::String("c".to_string()),
                ]),
            ],
        })
    );
}

#[test]
fn pipe_with_join_parsed() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = ["a", "b", "c"] |> join("-")
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    // ["a", "b", "c"] |> join("-") desugars to join("-", ["a", "b", "c"])
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                    Value::String("c".to_string()),
                ]),
            ],
        })
    );
}

#[test]
fn join_with_multiple_pipes() {
    // Chain: value |> f1(args) |> f2(args)
    let input = r#"
        let x = ["a", "b"] |> join("-") |> upper()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    // Pipe chaining: ["a", "b"] |> join("-") |> upper()
    // => upper(join("-", ["a", "b"]))
    assert_eq!(
        result.variables.get("x"),
        Some(&Value::FunctionCall {
            name: "upper".to_string(),
            args: vec![Value::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::String("-".to_string()),
                    Value::List(vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                    ]),
                ],
            }],
        })
    );
}

#[test]
fn function_call_with_no_args() {
    let input = r#"
        let x = foo()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("x"),
        Some(&Value::FunctionCall {
            name: "foo".to_string(),
            args: vec![],
        })
    );
}

#[test]
fn join_resolved_during_resource_ref_resolution() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = join("-", ["my", "bucket", "name"])
        }
    "#;
    let mut result = parse(input, &ProviderContext::default()).unwrap();
    resolve_resource_refs(&mut result).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("my-bucket-name".to_string()))
    );
}

#[test]
fn pipe_join_resolved_during_resource_ref_resolution() {
    let input = r#"
        let bucket = aws.s3_bucket {
            name = ["my", "bucket"] |> join("-")
        }
    "#;
    let mut result = parse(input, &ProviderContext::default()).unwrap();
    resolve_resource_refs(&mut result).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("my-bucket".to_string()))
    );
}

#[test]
fn partial_application_let_binding_dropped_from_variables() {
    // After #2230 a `let` binding holding a partial application
    // is an evaluator-only artifact: it lives on `EvalValue`
    // during parsing so a later pipe / call can finish it, but
    // it never reaches `ParsedFile.variables`. Parsing succeeds;
    // the binding simply does not appear in the user-facing
    // variable map.
    let input = r#"
        let f = map(".subnet_id")
    "#;
    let result = parse(input, &ProviderContext::default())
        .expect("partial application in let binding should parse");
    assert!(result.variables.get("f").is_none());
}

#[test]
fn partial_application_join_with_pipe() {
    // `["a", "b"] |> join(",")` desugars to join(",", ["a","b"]) which is a full call.
    // At parse time it stays as FunctionCall; resolution evaluates it.
    let input = r#"
        let bucket = aws.s3_bucket {
            name = ["a", "b"] |> join(",")
        }
    "#;
    let mut result = parse(input, &ProviderContext::default()).unwrap();
    resolve_resource_refs(&mut result).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("a,b".to_string()))
    );
}

#[test]
fn partial_application_closure_direct_call() {
    // `let f = join(","); let x = f(["a", "b"])` should work
    let input = r#"
        let f = join(",")
        let x = f(["a", "b"])
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("x"),
        Some(&Value::String("a,b".to_string()))
    );
}

#[test]
fn partial_application_chained_pipes() {
    // `["a", "b"] |> join(",") |> upper()` — resolved via resource refs
    let input = r#"
        let bucket = aws.s3_bucket {
            name = ["a", "b"] |> join(",") |> upper()
        }
    "#;
    let mut result = parse(input, &ProviderContext::default()).unwrap();
    resolve_resource_refs(&mut result).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("A,B".to_string()))
    );
}

#[test]
fn partial_application_closure_pipe() {
    // `let f = join(","); let x = ["a", "b"] |> f()` should work
    let input = r#"
        let f = join(",")
        let x = ["a", "b"] |> f()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("x"),
        Some(&Value::String("a,b".to_string()))
    );
}

#[test]
fn partial_application_too_many_args_errors() {
    // Calling a closure with too many args should error
    let input = r#"
        let f = join(",")
        let x = f(["a", "b"], "extra")
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
}

#[test]
fn partial_application_replace() {
    // `replace` has arity 3, partial application with 2 args
    let input = r#"
        let dash_to_underscore = replace("-", "_")
        let x = "hello-world" |> dash_to_underscore()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("x"),
        Some(&Value::String("hello_world".to_string()))
    );
}

#[test]
fn partial_application_in_resource_attribute() {
    // Partial application in a resource attribute via pipe
    let input = r#"
        let bucket = aws.s3_bucket {
            name = ["my", "bucket"] |> join("-")
        }
    "#;
    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    resolve_resource_refs(&mut parsed).unwrap();
    assert_eq!(
        parsed.resources[0].get_attr("name"),
        Some(&Value::String("my-bucket".to_string()))
    );
}

#[test]
fn partial_application_closure_in_resource_attribute() {
    // Closure variable used in resource attribute via pipe
    let input = r#"
        let dash_join = join("-")
        let bucket = aws.s3_bucket {
            name = ["my", "bucket"] |> dash_join()
        }
    "#;
    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    resolve_resource_refs(&mut parsed).unwrap();
    assert_eq!(
        parsed.resources[0].get_attr("name"),
        Some(&Value::String("my-bucket".to_string()))
    );
}

#[test]
fn partial_application_closure_direct_call_in_resource_attribute() {
    // Closure variable used in resource attribute via direct call
    let input = r#"
        let dash_join = join("-")
        let bucket = aws.s3_bucket {
            name = dash_join(["my", "bucket"])
        }
    "#;
    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    resolve_resource_refs(&mut parsed).unwrap();
    assert_eq!(
        parsed.resources[0].get_attr("name"),
        Some(&Value::String("my-bucket".to_string()))
    );
}

#[test]
fn forward_reference_parsed_as_resource_ref() {
    // Issue #866: Forward references should be resolved as ResourceRef,
    // not silently left as a plain string.
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            vpc_id     = vpc.vpc_id
            cidr_block = "10.0.1.0/24"
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    let subnet = &result.resources[0];
    // Forward reference vpc.vpc_id should be a ResourceRef, not a plain String
    assert_eq!(
        subnet.get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "vpc".to_string(),
            "vpc_id".to_string(),
            vec![]
        )),
        "Forward reference should be parsed as ResourceRef, got: {:?}",
        subnet.get_attr("vpc_id")
    );
}

#[test]
fn forward_reference_resolve_works() {
    // Issue #866: parse_and_resolve should work with forward references
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            vpc_id     = vpc.vpc_id
            cidr_block = "10.0.1.0/24"
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    // parse_and_resolve should not error on forward references
    let result = parse_and_resolve(input);
    assert!(
        result.is_ok(),
        "parse_and_resolve should succeed with forward references, got: {:?}",
        result.err()
    );
}

#[test]
fn forward_reference_unused_binding_detection() {
    // Forward-referenced bindings should be detected as used
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            vpc_id     = vpc.vpc_id
            cidr_block = "10.0.1.0/24"
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let unused = crate::validation::check_unused_bindings(&parsed);
    // vpc is referenced by subnet, so should NOT be unused
    assert!(
        !unused.contains(&"vpc".to_string()),
        "vpc should not be unused, but check_unused_bindings returned: {:?}",
        unused
    );
}

#[test]
fn forward_reference_in_nested_value() {
    // Forward references inside list/map values should also be resolved
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            vpc_id     = vpc.vpc_id
            cidr_block = "10.0.1.0/24"
            tags = [{ vpc_ref = vpc.vpc_id }]
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];
    // Check nested reference in list > map
    if let Some(Value::List(items)) = subnet.get_attr("tags") {
        if let Some(Value::Map(map)) = items.first() {
            assert_eq!(
                map.get("vpc_ref"),
                Some(&Value::resource_ref(
                    "vpc".to_string(),
                    "vpc_id".to_string(),
                    vec![]
                )),
                "Nested forward reference should be resolved"
            );
        } else {
            panic!("Expected map in tags list");
        }
    } else {
        panic!("Expected tags to be a list");
    }
}

#[test]
fn forward_reference_chained_three_parts() {
    // Issue #1259: Chained forward references like "later.attr.nested" should
    // be resolved to ResourceRef with field_path, not left as a plain string.
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            vpc_id     = vpc.encryption_specification.status
            cidr_block = "10.0.1.0/24"
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];
    assert_eq!(
        subnet.get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "vpc".to_string(),
            "encryption_specification".to_string(),
            vec!["status".to_string()]
        )),
        "Chained forward reference should be parsed as ResourceRef with field_path"
    );
}

#[test]
fn forward_reference_chained_four_parts() {
    // Issue #1259: Deep chained forward references like "later.attr.deep.nested"
    // should be resolved to ResourceRef with multiple field_path entries.
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            vpc_id     = vpc.config.deep.nested
            cidr_block = "10.0.1.0/24"
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];
    assert_eq!(
        subnet.get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "vpc".to_string(),
            "config".to_string(),
            vec!["deep".to_string(), "nested".to_string()]
        )),
        "Deep chained forward reference should have multiple field_path entries"
    );
}

#[test]
fn duplicate_let_binding_resource_produces_error() {
    // Issue #915: Duplicate let bindings should produce an error,
    // not silently overwrite the first binding.
    let input = r#"
        let rt = awscc.ec2.RouteTable {
            vpc_id = "vpc-123"
        }

        let rt = awscc.ec2.RouteTable {
            vpc_id = "vpc-456"
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "Duplicate let binding 'rt' should produce an error, but parsing succeeded: {:?}",
        result.unwrap()
    );
    let err = result.unwrap_err();
    match &err {
        ParseError::DuplicateBinding { name, line } => {
            assert_eq!(name, "rt");
            assert_eq!(
                *line, 6,
                "Duplicate binding should report the line of the second 'let rt', got line {line}"
            );
        }
        _ => panic!("Expected DuplicateBinding error, got: {err}"),
    }
    let err_str = err.to_string();
    assert!(
        err_str.contains("Duplicate") && err_str.contains("rt"),
        "Error should mention duplicate binding 'rt', got: {err_str}"
    );
}

#[test]
fn duplicate_let_binding_variable_produces_error() {
    // Issue #915: Duplicate variable bindings should also produce an error.
    let input = r#"
        let region = aws.Region.ap_northeast_1
        let region = aws.Region.us_east_1
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "Duplicate let binding 'region' should produce an error, but parsing succeeded: {:?}",
        result.unwrap()
    );
    let err = result.unwrap_err();
    match &err {
        ParseError::DuplicateBinding { name, line } => {
            assert_eq!(name, "region");
            assert_eq!(
                *line, 3,
                "Duplicate binding should report the line of the second 'let region', got line {line}"
            );
        }
        _ => panic!("Expected DuplicateBinding error, got: {err}"),
    }
    let err_str = err.to_string();
    assert!(
        err_str.contains("Duplicate") && err_str.contains("region"),
        "Error should mention duplicate binding 'region', got: {err_str}"
    );
}

#[test]
fn distinct_let_bindings_are_accepted() {
    // Sanity check: different binding names should work fine
    let input = r#"
        let rt1 = awscc.ec2.RouteTable {
            vpc_id = "vpc-123"
        }

        let rt2 = awscc.ec2.RouteTable {
            vpc_id = "vpc-456"
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "Distinct let bindings should parse successfully, got: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().resources.len(), 2);
}

#[test]
fn parse_error_has_internal_error_variant() {
    // Verify the InternalError variant exists and formats correctly
    let err = ParseError::InternalError {
        expected: "identifier".to_string(),
        context: "provider block".to_string(),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("expected identifier in provider block"),
        "InternalError should format with expected and context, got: {msg}"
    );
}

#[test]
fn parse_slash_slash_comment_standalone() {
    let input = r#"
        // This is a C-style comment
        provider aws {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].name, "aws");
}

#[test]
fn parse_slash_slash_comment_inline() {
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"  // inline comment
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
}

#[test]
fn parse_mixed_comment_styles() {
    let input = r#"
        # shell-style comment
        // C-style comment
        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"  // inline C-style
            tags = { Name = "main" }    # inline shell-style
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
}

#[test]
fn parse_block_comment_single_line() {
    let input = r#"
        /* single line block comment */
        provider aws {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].name, "aws");
}

#[test]
fn parse_block_comment_multi_line() {
    let input = r#"
        /*
          Multi-line block comment.
          All content is ignored by the parser.
        */
        provider aws {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].name, "aws");
}

#[test]
fn parse_block_comment_nested() {
    let input = r#"
        /* outer
          /* inner comment */
          still commented out
        */
        provider aws {
            region = aws.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].name, "aws");
}

#[test]
fn parse_block_comment_inline() {
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            cidr_block = /* inline block comment */ "10.0.0.0/16"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
}

#[test]
fn parse_block_comment_with_all_comment_styles() {
    let input = r#"
        # shell-style comment
        // C-style comment
        /* block comment */
        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"  // inline C-style
            tags = { Name = "main" }    # inline shell-style
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
}

#[test]
fn parse_provider_block_with_default_tags() {
    let input = r#"
        provider awscc {
            region = awscc.Region.ap_northeast_1
            default_tags = {
                Environment = "production"
                Team        = "platform"
                ManagedBy   = "carina"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].name, "awscc");
    // default_tags should be extracted from attributes
    assert!(!result.providers[0].attributes.contains_key("default_tags"));
    assert_eq!(result.providers[0].default_tags.len(), 3);
    assert_eq!(
        result.providers[0].default_tags.get("Environment"),
        Some(&Value::String("production".to_string()))
    );
    assert_eq!(
        result.providers[0].default_tags.get("Team"),
        Some(&Value::String("platform".to_string()))
    );
    assert_eq!(
        result.providers[0].default_tags.get("ManagedBy"),
        Some(&Value::String("carina".to_string()))
    );
}

#[test]
fn parse_provider_block_without_default_tags() {
    let input = r#"
        provider awscc {
            region = awscc.Region.ap_northeast_1
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert!(result.providers[0].default_tags.is_empty());
}

#[test]
fn parse_provider_block_with_source_and_version() {
    let input = r#"
        provider mock {
            source = "github.com/carina-rs/carina-provider-mock"
            version = "0.1.0"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.providers.len(), 1);

    let provider = &parsed.providers[0];
    assert_eq!(provider.name, "mock");
    assert_eq!(
        provider.source.as_deref(),
        Some("github.com/carina-rs/carina-provider-mock")
    );
    assert_eq!(provider.version.as_ref().unwrap().raw, "0.1.0");
    // source and version should NOT be in attributes
    assert!(!provider.attributes.contains_key("source"));
    assert!(!provider.attributes.contains_key("version"));
}

#[test]
fn parse_provider_block_without_source() {
    let input = r#"
        provider awscc {
            region = awscc.Region.ap_northeast_1
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let provider = &parsed.providers[0];
    assert!(provider.source.is_none());
    assert!(provider.version.is_none());
}

#[test]
fn parse_provider_block_with_version_constraint() {
    let input = r#"
        provider mock {
            source = "github.com/carina-rs/carina-provider-mock"
            version = "~0.5.0"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let provider = &parsed.providers[0];
    let vc = provider.version.as_ref().unwrap();
    assert_eq!(vc.raw, "~0.5.0");
    assert!(vc.matches("0.5.3").unwrap());
    assert!(!vc.matches("0.6.0").unwrap());
}

#[test]
fn parse_provider_block_with_invalid_version_constraint() {
    let input = r#"
        provider mock {
            source = "github.com/carina-rs/carina-provider-mock"
            version = "not-valid"
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
}

#[test]
fn parse_provider_block_with_revision() {
    let input = r#"
        provider mock {
            source = "github.com/carina-rs/carina-provider-mock"
            revision = "feature-branch"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.providers.len(), 1);

    let provider = &parsed.providers[0];
    assert_eq!(provider.name, "mock");
    assert_eq!(
        provider.source.as_deref(),
        Some("github.com/carina-rs/carina-provider-mock")
    );
    assert_eq!(provider.revision.as_deref(), Some("feature-branch"));
    assert!(provider.version.is_none());
    assert!(!provider.attributes.contains_key("revision"));
}

#[test]
fn parse_provider_block_with_revision_sha() {
    let input = r#"
        provider mock {
            source = "github.com/carina-rs/carina-provider-mock"
            revision = "abc123def456"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let provider = &parsed.providers[0];
    assert_eq!(provider.revision.as_deref(), Some("abc123def456"));
}

#[test]
fn parse_provider_block_version_and_revision_mutually_exclusive() {
    let input = r#"
        provider mock {
            source = "github.com/carina-rs/carina-provider-mock"
            version = "0.1.0"
            revision = "main"
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("mutually exclusive"),
        "Error should mention mutual exclusivity, got: {err}"
    );
}

#[test]
fn resolve_resource_refs_with_argument_parameters() {
    let input = r#"
        arguments {
            cidr_block: String
            subnet_cidr: String
            az: String
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = cidr_block
        }

        let subnet = awscc.ec2.Subnet {
            vpc_id = vpc.vpc_id
            cidr_block = subnet_cidr
            availability_zone = az
        }

        attributes {
            vpc_id: awscc.ec2.Vpc = vpc.vpc_id
        }
    "#;

    // parse_and_resolve should succeed without "Undefined variable" errors
    let result = parse_and_resolve(input);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());

    let parsed = result.unwrap();
    assert_eq!(parsed.resources.len(), 2); // allow: direct — fixture test inspection
    assert_eq!(parsed.arguments.len(), 3);
}

#[test]
fn parse_let_binding_module_call() {
    let input = r#"
        let web_tier = use { source = "./modules/web_tier" }

        let web = web_tier {
            vpc = "vpc-123"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.module_calls.len(), 1);

    let call = &result.module_calls[0];
    assert_eq!(call.module_name, "web_tier");
    assert_eq!(call.binding_name, Some("web".to_string()));
    assert_eq!(
        call.arguments.get("vpc"),
        Some(&Value::String("vpc-123".to_string()))
    );
}

#[test]
fn parse_module_call_binding_enables_resource_ref() {
    // After `let web = web_tier { ... }`, `web.security_group` should
    // resolve as ResourceRef.
    let input = r#"
        let web_tier = use { source = "./modules/web_tier" }

        let web = web_tier {
            vpc = "vpc-123"
        }

        let sg = awscc.ec2.SecurityGroup {
            group_description = "test"
            group_name = web.security_group
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let sg = &result.resources[0];
    assert_eq!(
        sg.get_attr("group_name"),
        Some(&Value::resource_ref(
            "web".to_string(),
            "security_group".to_string(),
            vec![]
        ))
    );
}

#[test]
fn parse_string_interpolation_simple() {
    let input = r#"
        let env = "prod"
        let vpc = aws.ec2.Vpc {
            name = "vpc-${env}"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("vpc-prod".to_string()))
    );
}

#[test]
fn parse_string_interpolation_multiple_exprs() {
    let input = r#"
        let env = "prod"
        let region = "us-east-1"
        let vpc = aws.ec2.Vpc {
            name = "vpc-${env}-${region}"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("vpc-prod-us-east-1".to_string()))
    );
}

#[test]
fn parse_string_interpolation_with_resource_ref() {
    let input = r#"
        let vpc = aws.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
        let subnet = aws.ec2.Subnet {
            name = "subnet-${vpc.vpc_id}"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[1];
    assert_eq!(
        subnet.get_attr("name"),
        Some(&Value::Interpolation(vec![
            InterpolationPart::Literal("subnet-".to_string()),
            InterpolationPart::Expr(Value::resource_ref(
                "vpc".to_string(),
                "vpc_id".to_string(),
                vec![]
            )),
        ]))
    );
}

#[test]
fn parse_string_no_interpolation() {
    // Strings without ${} should remain as plain Value::String
    let input = r#"
        let vpc = aws.ec2.Vpc {
            name = "my-vpc"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("my-vpc".to_string()))
    );
}

#[test]
fn parse_string_dollar_without_brace() {
    // A $ not followed by { should be literal
    let input = r#"
        let vpc = aws.ec2.Vpc {
            name = "price$100"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("price$100".to_string()))
    );
}

#[test]
fn parse_string_escaped_interpolation() {
    // \${ should be literal ${
    let input = r#"
        let vpc = aws.ec2.Vpc {
            name = "literal\${expr}"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("literal${expr}".to_string()))
    );
}

#[test]
fn parse_string_interpolation_with_bool() {
    let input = r#"
        let vpc = aws.ec2.Vpc {
            name = "enabled-${true}"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("enabled-true".to_string()))
    );
}

#[test]
fn parse_string_interpolation_with_number() {
    let input = r#"
        let vpc = aws.ec2.Vpc {
            name = "port-${8080}"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("port-8080".to_string()))
    );
}

#[test]
fn parse_string_interpolation_only_expr() {
    // String with only interpolation, no literal parts
    let input = r#"
        let name = "prod"
        let vpc = aws.ec2.Vpc {
            tag = "${name}"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("tag"),
        Some(&Value::String("prod".to_string()))
    );
}

#[test]
fn parse_local_let_binding_in_resource_block() {
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            let name = "my-subnet"
            cidr_block = "10.0.1.0/24"
            tag_name = name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];

    // Local let binding should NOT appear in attributes
    assert!(!subnet.attributes.contains_key("name"));

    // The local binding value should be resolved in subsequent attributes
    assert_eq!(
        subnet.get_attr("tag_name"),
        Some(&Value::String("my-subnet".to_string()))
    );
    assert_eq!(
        subnet.get_attr("cidr_block"),
        Some(&Value::String("10.0.1.0/24".to_string()))
    );
}

#[test]
fn parse_local_let_binding_with_interpolation() {
    let input = r#"
        let env = "prod"
        let subnet = awscc.ec2.Subnet {
            let name = "app-${env}"
            cidr_block = "10.0.1.0/24"
            tag_name = name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];

    // Local binding should resolve outer scope variable in interpolation.
    assert_eq!(
        subnet.get_attr("tag_name"),
        Some(&Value::String("app-prod".to_string()))
    );
}

#[test]
fn parse_local_let_binding_chain() {
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            let prefix = "app"
            let name = "${prefix}-subnet"
            cidr_block = "10.0.1.0/24"
            tag_name = name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];

    // Chained local bindings should resolve correctly.
    assert_eq!(
        subnet.get_attr("tag_name"),
        Some(&Value::String("app-subnet".to_string()))
    );

    // Local bindings should NOT appear in attributes
    assert!(!subnet.attributes.contains_key("prefix"));
    assert!(!subnet.attributes.contains_key("name"));
}

#[test]
fn parse_local_let_binding_with_function_call() {
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            let name = "my-subnet"
            cidr_block = "10.0.1.0/24"
            tag_name = upper(name)
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];

    // Local binding used inside function call
    assert_eq!(
        subnet.get_attr("tag_name"),
        Some(&Value::FunctionCall {
            name: "upper".to_string(),
            args: vec![Value::String("my-subnet".to_string())],
        })
    );
}

#[test]
fn parse_local_let_binding_in_anonymous_resource() {
    let input = r#"
        awscc.ec2.Subnet {
            let name = "my-subnet"
            cidr_block = "10.0.1.0/24"
            tag_name = name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];

    // Local let binding should work in anonymous resources too
    assert!(!subnet.attributes.contains_key("name"));
    assert_eq!(
        subnet.get_attr("tag_name"),
        Some(&Value::String("my-subnet".to_string()))
    );
}

#[test]
fn parse_local_let_binding_in_nested_block() {
    let input = r#"
        let subnet = awscc.ec2.Subnet {
            let env = "prod"
            cidr_block = "10.0.1.0/24"
            tags {
                Name = env
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[0];

    // Local binding should be visible in nested blocks
    if let Some(Value::List(tags_list)) = subnet.get_attr("tags") {
        if let Some(Value::Map(tags)) = tags_list.first() {
            assert_eq!(tags.get("Name"), Some(&Value::String("prod".to_string())));
        } else {
            panic!("Expected Map in tags list");
        }
    } else {
        panic!("Expected tags attribute as List");
    }
}

#[test]
fn parse_for_expression_over_list() {
    let input = r#"
        let subnets = for az in ["ap-northeast-1a", "ap-northeast-1c"] {
            awscc.ec2.Subnet {
                availability_zone = az
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    // for expression expands to individual resources at parse time
    assert_eq!(result.resources.len(), 2);

    // Resources should be addressed as subnets[0] and subnets[1]
    assert_eq!(result.resources[0].id.name_str(), "subnets[0]");
    assert_eq!(result.resources[1].id.name_str(), "subnets[1]");

    // Each resource should have the loop variable substituted
    assert_eq!(
        result.resources[0].get_attr("availability_zone"),
        Some(&Value::String("ap-northeast-1a".to_string()))
    );
    assert_eq!(
        result.resources[1].get_attr("availability_zone"),
        Some(&Value::String("ap-northeast-1c".to_string()))
    );
}

#[test]
fn parse_for_expression_with_index() {
    let input = r#"
        let subnets = for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
            awscc.ec2.Subnet {
                availability_zone = az
                cidr_block = cidr_subnet("10.0.0.0/16", 8, i)
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    assert_eq!(result.resources[0].id.name_str(), "subnets[0]");
    assert_eq!(result.resources[1].id.name_str(), "subnets[1]");

    // Check index variable is substituted
    if let Some(Value::FunctionCall { args, .. }) = result.resources[0].get_attr("cidr_block") {
        assert_eq!(args[2], Value::Int(0));
    } else {
        panic!("Expected FunctionCall for cidr_block");
    }

    if let Some(Value::FunctionCall { args, .. }) = result.resources[1].get_attr("cidr_block") {
        assert_eq!(args[2], Value::Int(1));
    } else {
        panic!("Expected FunctionCall for cidr_block");
    }
}

#[test]
fn parse_for_expression_over_map() {
    let input = r#"
        let cidrs = {
            prod    = "10.0.0.0/16"
            staging = "10.1.0.0/16"
        }

        let networks = for name, cidr in cidrs {
            awscc.ec2.Vpc {
                cidr_block = cidr
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    // Map iteration produces map-keyed addresses in canonical dot
    // form (#1903) — both keys here are identifier-safe.
    let names: Vec<&str> = result
        .resources
        .iter()
        .map(|r| r.id.name.as_str())
        .collect();
    assert!(names.contains(&"networks.prod"));
    assert!(names.contains(&"networks.staging"));
}

#[test]
fn parse_for_expression_with_local_binding() {
    let input = r#"
        let subnets = for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
            let cidr = cidr_subnet("10.0.0.0/16", 8, i)
            awscc.ec2.Subnet {
                cidr_block = cidr
                availability_zone = az
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    // Local binding should be resolved within each iteration
    if let Some(Value::FunctionCall { name, args }) = result.resources[0].get_attr("cidr_block") {
        assert_eq!(name, "cidr_subnet");
        assert_eq!(args[2], Value::Int(0));
    } else {
        panic!("Expected FunctionCall for cidr_block");
    }
}

#[test]
fn parse_for_expression_with_module_call() {
    let input = r#"
        let web = use { source = "modules/web" }

        let envs = {
            prod    = "10.0.0.0/16"
            staging = "10.1.0.0/16"
        }

        let webs = for name, cidr in envs {
            web { vpc_cidr = cidr }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    // for expression with module call should produce module calls, not resources
    assert_eq!(result.module_calls.len(), 2);

    // Module calls have canonical dot-form binding names (#1903) —
    // both keys here are identifier-safe.
    let binding_names: Vec<&str> = result
        .module_calls
        .iter()
        .map(|c| c.binding_name.as_deref().unwrap())
        .collect();
    assert!(binding_names.contains(&"webs.prod"));
    assert!(binding_names.contains(&"webs.staging"));

    // Each module call should have the loop variable substituted in arguments
    for call in &result.module_calls {
        assert_eq!(call.module_name, "web");
        assert!(call.arguments.contains_key("vpc_cidr"));
    }

    // Verify the argument values are the substituted loop values
    let prod_call = result
        .module_calls
        .iter()
        .find(|c| c.binding_name.as_deref() == Some("webs.prod"))
        .unwrap();
    assert_eq!(
        prod_call.arguments.get("vpc_cidr"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );

    let staging_call = result
        .module_calls
        .iter()
        .find(|c| c.binding_name.as_deref() == Some("webs.staging"))
        .unwrap();
    assert_eq!(
        staging_call.arguments.get("vpc_cidr"),
        Some(&Value::String("10.1.0.0/16".to_string()))
    );
}

#[test]
fn parse_for_expression_with_module_call_over_list() {
    let input = r#"
        let web = use { source = "modules/web" }

        let webs = for cidr in ["10.0.0.0/16", "10.1.0.0/16"] {
            web { vpc_cidr = cidr }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();

    // for expression with module call over list
    assert_eq!(result.module_calls.len(), 2);
    assert_eq!(result.resources.len(), 0);

    assert_eq!(
        result.module_calls[0].binding_name.as_deref(),
        Some("webs[0]")
    );
    assert_eq!(
        result.module_calls[1].binding_name.as_deref(),
        Some("webs[1]")
    );

    assert_eq!(
        result.module_calls[0].arguments.get("vpc_cidr"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
    assert_eq!(
        result.module_calls[1].arguments.get("vpc_cidr"),
        Some(&Value::String("10.1.0.0/16".to_string()))
    );
}

#[test]
fn test_chained_field_access_two_levels() {
    // a.b.c should parse as ResourceRef with binding_name="a", attribute_name="b", field_path=["c"]
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            name = "test-vpc"
        }

        awscc.ec2.Subnet {
            name = "test-subnet"
            vpc_id = vpc.network.vpc_id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[1];
    let vpc_id = subnet.get_attr("vpc_id").expect("vpc_id attribute");
    match vpc_id {
        Value::ResourceRef { path } => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let field_path = path.field_path();
            assert_eq!(binding_name, "vpc");
            assert_eq!(attribute_name, "network");
            assert_eq!(field_path, vec!["vpc_id"]);
        }
        other => panic!("Expected ResourceRef with field_path, got {:?}", other),
    }
}

#[test]
fn test_chained_field_access_three_levels() {
    // a.b.c.d should parse as ResourceRef with binding_name="a", attribute_name="b", field_path=["c", "d"]
    let input = r#"
        let web = awscc.ec2.Vpc {
            name = "test"
        }

        awscc.ec2.Subnet {
            name = "test-subnet"
            vpc_id = web.output.network.vpc_id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = &result.resources[1];
    let vpc_id = subnet.get_attr("vpc_id").expect("vpc_id attribute");
    match vpc_id {
        Value::ResourceRef { path } => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let field_path = path.field_path();
            assert_eq!(binding_name, "web");
            assert_eq!(attribute_name, "output");
            assert_eq!(field_path, vec!["network", "vpc_id"]);
        }
        other => panic!("Expected ResourceRef with field_path, got {:?}", other),
    }
}

#[test]
fn parse_index_access_with_integer() {
    // subnets[0].subnet_id should parse as ResourceRef with binding_name="subnets[0]"
    let input = r#"
        let subnets = for az in ["ap-northeast-1a", "ap-northeast-1c"] {
            awscc.ec2.Subnet {
                availability_zone = az
            }
        }

        awscc.ec2.RouteTable {
            name = "test"
            subnet_id = subnets[0].subnet_id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let rt = result.resources.last().expect("route_table resource");
    let subnet_id = rt.get_attr("subnet_id").expect("subnet_id attribute");
    match subnet_id {
        Value::ResourceRef { path } => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let field_path = path.field_path();
            assert_eq!(binding_name, "subnets[0]");
            assert_eq!(attribute_name, "subnet_id");
            assert!(field_path.is_empty());
        }
        other => panic!("Expected ResourceRef, got {:?}", other),
    }
}

#[test]
fn parse_index_access_with_string_key() {
    // `networks["prod"].vpc_id` parses as a ResourceRef whose binding
    // name is the canonical dot form `networks.prod` (#1903) — the
    // index-access syntax with an identifier-safe string key
    // collapses to the same address that `for`-iteration emits.
    let input = r#"
        let cidrs = {
            prod    = "10.0.0.0/16"
            staging = "10.1.0.0/16"
        }

        let networks = for name, cidr in cidrs {
            awscc.ec2.Vpc {
                cidr_block = cidr
            }
        }

        awscc.ec2.Subnet {
            name = "test"
            vpc_id = networks["prod"].vpc_id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = result.resources.last().expect("subnet resource");
    let vpc_id = subnet.get_attr("vpc_id").expect("vpc_id attribute");
    match vpc_id {
        Value::ResourceRef { path } => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let field_path = path.field_path();
            assert_eq!(binding_name, "networks.prod");
            assert_eq!(attribute_name, "vpc_id");
            assert!(field_path.is_empty());
        }
        other => panic!("Expected ResourceRef, got {:?}", other),
    }
}

#[test]
fn parse_index_access_with_chained_fields() {
    // webs["prod"].security_group.id should parse with field_path
    let input = r#"
        let cidrs = {
            prod    = "10.0.0.0/16"
            staging = "10.1.0.0/16"
        }

        let webs = for name, cidr in cidrs {
            awscc.ec2.Vpc {
                cidr_block = cidr
            }
        }

        awscc.ec2.Subnet {
            name = "test"
            sg_id = webs["prod"].security_group.id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = result.resources.last().expect("subnet resource");
    let sg_id = subnet.get_attr("sg_id").expect("sg_id attribute");
    match sg_id {
        Value::ResourceRef { path } => {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let field_path = path.field_path();
            assert_eq!(binding_name, "webs.prod");
            assert_eq!(attribute_name, "security_group");
            assert_eq!(field_path, vec!["id"]);
        }
        other => panic!("Expected ResourceRef with field_path, got {:?}", other),
    }
}

#[test]
fn parse_subscript_after_field_access_with_integer() {
    // `orgs.accounts[0]` — subscript after `binding.field`. Issue #2318.
    let input = r#"
        let orgs = upstream_state { source = "../organizations" }

        awscc.ec2.Subnet {
            name = "test"
            cidr_block = orgs.accounts[0]
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = result.resources.last().expect("subnet");
    let value = subnet.get_attr("cidr_block").expect("cidr_block");
    match value {
        Value::ResourceRef { path } => {
            assert_eq!(path.binding(), "orgs");
            assert_eq!(path.attribute(), "accounts");
            assert!(path.field_path().is_empty());
            assert_eq!(
                path.subscripts(),
                [crate::resource::Subscript::Int { index: 0 }]
            );
            assert_eq!(path.to_dot_string(), "orgs.accounts[0]");
        }
        other => panic!("Expected ResourceRef with subscript, got {:?}", other),
    }
}

#[test]
fn parse_subscript_after_field_access_with_string_key() {
    // `orgs.accounts["alpha"]` — subscript after `binding.field`. Issue #2318.
    let input = r#"
        let orgs = upstream_state { source = "../organizations" }

        awscc.ec2.Subnet {
            name = "test"
            cidr_block = orgs.accounts["alpha"]
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = result.resources.last().expect("subnet");
    let value = subnet.get_attr("cidr_block").expect("cidr_block");
    match value {
        Value::ResourceRef { path } => {
            assert_eq!(path.binding(), "orgs");
            assert_eq!(path.attribute(), "accounts");
            assert!(path.field_path().is_empty());
            assert_eq!(
                path.subscripts(),
                [crate::resource::Subscript::Str {
                    key: "alpha".to_string()
                }]
            );
            assert_eq!(path.to_dot_string(), "orgs.accounts[\"alpha\"]");
        }
        other => panic!("Expected ResourceRef with subscript, got {:?}", other),
    }
}

#[test]
fn parse_chained_subscripts_after_field_access() {
    // `orgs.matrix[0][1]` — multiple subscripts after field access.
    // The shape check relies on the AST exposing them in source order.
    let input = r#"
        let orgs = upstream_state { source = "../organizations" }

        awscc.ec2.Subnet {
            name = "test"
            cidr_block = orgs.matrix[0][1]
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = result.resources.last().expect("subnet");
    let value = subnet.get_attr("cidr_block").expect("cidr_block");
    match value {
        Value::ResourceRef { path } => {
            assert_eq!(path.binding(), "orgs");
            assert_eq!(path.attribute(), "matrix");
            assert!(path.field_path().is_empty());
            assert_eq!(
                path.subscripts(),
                [
                    crate::resource::Subscript::Int { index: 0 },
                    crate::resource::Subscript::Int { index: 1 },
                ]
            );
        }
        other => panic!("Expected ResourceRef, got {:?}", other),
    }
}

#[test]
fn parse_negative_subscript_is_rejected() {
    // `orgs.accounts[-1]` — the DSL has no `[-1]` "from end" semantic.
    // Rejecting at parse time avoids the validator passing and the
    // resolver silently falling back to an unresolved ref.
    let input = r#"
        let orgs = upstream_state { source = "../organizations" }

        awscc.ec2.Subnet {
            name = "test"
            cidr_block = orgs.accounts[-1]
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("non-negative"),
        "expected non-negative rejection, got: {msg}"
    );
}

#[test]
fn parse_field_after_subscript_after_field_is_rejected() {
    // `a.b[0].c` — once a subscript appears after a field, no more
    // fields are allowed. Runtime list-indexing of arbitrary structs
    // isn't representable today.
    let input = r#"
        let orgs = upstream_state { source = "../organizations" }

        awscc.ec2.Subnet {
            name = "test"
            cidr_block = orgs.accounts[0].id
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("field access after index access")
            || msg.contains("field_access after index_access")
            || msg.contains("InvalidExpression")
            || msg.contains("Syntax"),
        "expected rejection of `a.b[0].c`, got: {msg}"
    );
}

#[test]
fn parse_subscript_after_nested_field_access() {
    // `orgs.account.accounts[0]` — subscript after a multi-level field
    // chain. Should populate both `field_path` and `subscripts`.
    let input = r#"
        let orgs = upstream_state { source = "../organizations" }

        awscc.ec2.Subnet {
            name = "test"
            cidr_block = orgs.account.accounts[0]
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let subnet = result.resources.last().expect("subnet");
    let value = subnet.get_attr("cidr_block").expect("cidr_block");
    match value {
        Value::ResourceRef { path } => {
            assert_eq!(path.binding(), "orgs");
            assert_eq!(path.attribute(), "account");
            assert_eq!(path.field_path(), vec!["accounts"]);
            assert_eq!(
                path.subscripts(),
                [crate::resource::Subscript::Int { index: 0 }]
            );
        }
        other => panic!("Expected ResourceRef, got {:?}", other),
    }
}

#[test]
fn parse_import_block() {
    let input = r#"
        import {
            to = awscc.ec2.Vpc "main-vpc"
            id = "vpc-0abc123def456"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.state_blocks.len(), 1);
    match &result.state_blocks[0] {
        StateBlock::Import { to, id } => {
            assert_eq!(to.provider, "awscc");
            assert_eq!(to.resource_type, "ec2.Vpc");
            assert_eq!(to.name_str(), "main-vpc");
            assert_eq!(id, "vpc-0abc123def456");
        }
        other => panic!("Expected Import, got {:?}", other),
    }
}

#[test]
fn parse_removed_block() {
    let input = r#"
        removed {
            from = awscc.ec2.Vpc "legacy-vpc"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.state_blocks.len(), 1);
    match &result.state_blocks[0] {
        StateBlock::Removed { from } => {
            assert_eq!(from.provider, "awscc");
            assert_eq!(from.resource_type, "ec2.Vpc");
            assert_eq!(from.name_str(), "legacy-vpc");
        }
        other => panic!("Expected Removed, got {:?}", other),
    }
}

#[test]
fn parse_moved_block() {
    let input = r#"
        moved {
            from = awscc.ec2.Subnet "old-name"
            to   = awscc.ec2.Subnet "new-name"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.state_blocks.len(), 1);
    match &result.state_blocks[0] {
        StateBlock::Moved { from, to } => {
            assert_eq!(from.provider, "awscc");
            assert_eq!(from.resource_type, "ec2.Subnet");
            assert_eq!(from.name_str(), "old-name");
            assert_eq!(to.provider, "awscc");
            assert_eq!(to.resource_type, "ec2.Subnet");
            assert_eq!(to.name_str(), "new-name");
        }
        other => panic!("Expected Moved, got {:?}", other),
    }
}

#[test]
fn moved_block_accepts_three_map_key_address_forms() {
    // #1903: a `moved` block addresses a map-keyed resource. All three
    // input shapes — bare dot, single-quoted bracket, double-quoted
    // bracket — must collapse to the canonical form so existing state
    // (which may have been written under any historical shape) still
    // resolves.
    // The DSL has two string-literal forms. We pair each input shape
    // with the outer quoting that lets the inner shape sit unescaped:
    // - dot form: any outer
    // - `['key']`: outer `"`-delimited
    // - `["key"]`: outer `'`-delimited
    let cases = [
        (
            "dot",
            r#"to = awscc.sso.Assignment "_accounts.registry_prod""#,
        ),
        (
            "single-quote bracket",
            r#"to = awscc.sso.Assignment "_accounts['registry_prod']""#,
        ),
        (
            "double-quote bracket",
            r#"to = awscc.sso.Assignment '_accounts["registry_prod"]'"#,
        ),
    ];
    for (label, to_clause) in cases {
        let input = format!(
            r#"
                moved {{
                    from = awscc.sso.Assignment "_accounts[0]"
                    {}
                }}
            "#,
            to_clause
        );
        let result = parse(&input, &ProviderContext::default())
            .unwrap_or_else(|e| panic!("parse failed for {label}: {e:?}"));
        assert_eq!(result.state_blocks.len(), 1);
        match &result.state_blocks[0] {
            StateBlock::Moved { to, .. } => {
                assert_eq!(
                    to.name_str(),
                    "_accounts.registry_prod",
                    "input shape {label} must canonicalize to dot form",
                );
            }
            other => panic!("Expected Moved, got {:?}", other),
        }
    }
}

#[test]
fn moved_block_keeps_non_identifier_safe_key_in_quoted_form() {
    // Keys with hyphens, spaces, or leading digits are not
    // identifier-safe — the canonical form keeps them in single-quoted
    // brackets. Both legacy `["..."]` and `['...']` collapse to
    // single-quoted.
    let input = r#"
        moved {
            from = awscc.sso.Assignment "_accounts[0]"
            to   = awscc.sso.Assignment '_envs["prod-east"]'
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    match &result.state_blocks[0] {
        StateBlock::Moved { to, .. } => {
            assert_eq!(to.name_str(), "_envs['prod-east']");
        }
        other => panic!("Expected Moved, got {:?}", other),
    }
}

#[test]
fn for_expression_over_map_uses_canonical_dot_form() {
    // The emit side mirrors the canonicalizer: a map iteration where
    // every key is identifier-safe must produce `binding.key` addresses
    // — no embedded quotes — so `moved`/`removed` blocks targeting
    // those resources can stay quote-free.
    let input = r#"
        let envs = {
            prod = "p"
            dev  = "d"
        }

        let resources = for key, val in envs {
            awscc.ec2.Subnet {
                name = key
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let names: Vec<&str> = result.resources.iter().map(|r| r.id.name_str()).collect();
    assert_eq!(names, vec!["resources.dev", "resources.prod"]);
}

#[test]
fn parse_for_expression_with_keys_function_call() {
    let input = r#"
        let tags = {
            Name = "web"
            Env  = "prod"
        }

        let resources = for key in keys(tags) {
            awscc.ec2.Subnet {
                name = key
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    // keys({Name = "web", Env = "prod"}) should evaluate to ["Env", "Name"] (sorted)
    assert_eq!(result.resources.len(), 2);
    assert_eq!(result.resources[0].id.name_str(), "resources[0]");
    assert_eq!(result.resources[1].id.name_str(), "resources[1]");
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("Env".to_string()))
    );
    assert_eq!(
        result.resources[1].get_attr("name"),
        Some(&Value::String("Name".to_string()))
    );
}

#[test]
fn parse_for_expression_with_values_function_call() {
    let input = r#"
        let cidrs = {
            prod    = "10.0.0.0/16"
            staging = "10.1.0.0/16"
        }

        let networks = for cidr in values(cidrs) {
            awscc.ec2.Vpc {
                cidr_block = cidr
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    // values() returns values sorted by key: prod, staging
    assert_eq!(result.resources.len(), 2);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
    assert_eq!(
        result.resources[1].get_attr("cidr_block"),
        Some(&Value::String("10.1.0.0/16".to_string()))
    );
}

#[test]
fn parse_for_expression_with_concat_function_call() {
    let input = r#"
        let networks = for cidr in concat(["10.0.0.0/16"], ["10.1.0.0/16"]) {
            awscc.ec2.Vpc {
                cidr_block = cidr
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);
    // concat(items, base_list) => base_list ++ items
    // So concat(["10.0.0.0/16"], ["10.1.0.0/16"]) => ["10.1.0.0/16", "10.0.0.0/16"]
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.1.0.0/16".to_string()))
    );
    assert_eq!(
        result.resources[1].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
}

#[test]
fn parse_for_expression_with_runtime_function_call_errors() {
    // Function call with runtime-dependent args (ResourceRef) should error
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            name = "test"
        }

        let subnets = for key in keys(vpc.tags) {
            awscc.ec2.Subnet {
                name = key
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("runtime"),
        "Expected error about runtime dependency, got: {}",
        err
    );
}

// ── if/else expression tests ──

#[test]
fn parse_if_true_condition_includes_resource() {
    let input = r#"
        let alarm = if true {
            awscc.cloudwatch.alarm {
                alarm_name = "cpu-high"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id.name_str(), "alarm");
    assert_eq!(
        result.resources[0].get_attr("alarm_name"),
        Some(&Value::String("cpu-high".to_string()))
    );
}

#[test]
fn parse_if_false_condition_no_resource() {
    let input = r#"
        let alarm = if false {
            awscc.cloudwatch.alarm {
                alarm_name = "cpu-high"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 0);
}

#[test]
fn parse_if_else_true_uses_if_branch() {
    let input = r#"
        let vpc = if true {
            awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        } else {
            awscc.ec2.Vpc {
                cidr_block = "172.16.0.0/16"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
}

#[test]
fn parse_if_else_false_uses_else_branch() {
    let input = r#"
        let vpc = if false {
            awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        } else {
            awscc.ec2.Vpc {
                cidr_block = "172.16.0.0/16"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("172.16.0.0/16".to_string()))
    );
}

#[test]
fn parse_if_else_value_expression() {
    let input = r#"
        let instance_type = if true {
            "m5.xlarge"
        } else {
            "t3.micro"
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 0);
    // The binding should be set to the value from the true branch
    // We verify by using the variable in a resource
    let input2 = r#"
        let instance_type = if true {
            "m5.xlarge"
        } else {
            "t3.micro"
        }

        awscc.ec2.Instance {
            instance_type = instance_type
        }
    "#;

    let result2 = parse(input2, &ProviderContext::default()).unwrap();
    assert_eq!(
        result2.resources[0].get_attr("instance_type"),
        Some(&Value::String("m5.xlarge".to_string()))
    );
}

#[test]
fn parse_if_else_value_expression_false_branch() {
    let input = r#"
        let instance_type = if false {
            "m5.xlarge"
        } else {
            "t3.micro"
        }

        awscc.ec2.Instance {
            instance_type = instance_type
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("instance_type"),
        Some(&Value::String("t3.micro".to_string()))
    );
}

#[test]
fn parse_if_with_variable_condition() {
    let input = r#"
        let enable_monitoring = true

        let alarm = if enable_monitoring {
            awscc.cloudwatch.alarm {
                alarm_name = "cpu-high"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
}

#[test]
fn parse_if_non_bool_condition_errors() {
    let input = r#"
        let alarm = if "not_a_bool" {
            awscc.cloudwatch.alarm {
                alarm_name = "cpu-high"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Bool"),
        "Expected error about Bool condition, got: {}",
        err
    );
}

#[test]
fn parse_if_resource_ref_condition_errors() {
    let input = r#"
        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }

        let alarm = if vpc.enabled {
            awscc.cloudwatch.alarm {
                alarm_name = "cpu-high"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("runtime") || err.contains("statically"),
        "Expected error about runtime dependency, got: {}",
        err
    );
}

#[test]
fn parse_if_with_module_call() {
    let input = r#"
        let web = use { source = "modules/web" }

        let monitoring = if true {
            web { vpc_id = "vpc-123" }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.module_calls.len(), 1);
    assert_eq!(result.module_calls[0].module_name, "web");
}

#[test]
fn parse_if_false_with_module_call() {
    let input = r#"
        let web = use { source = "modules/web" }

        let monitoring = if false {
            web { vpc_id = "vpc-123" }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.module_calls.len(), 0);
}

#[test]
fn parse_if_with_local_binding() {
    let input = r#"
        let alarm = if true {
            let name = "cpu-high"
            awscc.cloudwatch.alarm {
                alarm_name = name
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("alarm_name"),
        Some(&Value::String("cpu-high".to_string()))
    );
}

#[test]
fn parse_if_else_value_expr_in_attribute_true() {
    let input = r#"
        let is_production = true

        awscc.ec2.Vpc {
            cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
}

#[test]
fn parse_if_else_value_expr_in_attribute_false() {
    let input = r#"
        let is_production = false

        awscc.ec2.Vpc {
            cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("172.16.0.0/16".to_string()))
    );
}

#[test]
fn parse_if_value_expr_no_else_true() {
    // When condition is true and no else, the value is used
    let input = r#"
        awscc.ec2.Vpc {
            cidr_block = if true { "10.0.0.0/16" }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("cidr_block"),
        Some(&Value::String("10.0.0.0/16".to_string()))
    );
}

#[test]
fn parse_if_value_expr_no_else_false_errors() {
    // When condition is false and no else, it's an error in value position
    let input = r#"
        awscc.ec2.Vpc {
            cidr_block = if false { "10.0.0.0/16" }
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("else"),
        "Expected error about missing else clause, got: {}",
        err
    );
}

#[test]
fn parse_top_level_for_expression() {
    let input = r#"
        for az in ["ap-northeast-1a", "ap-northeast-1c"] {
            awscc.ec2.Subnet {
                availability_zone = az
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    // Each resource should have the loop variable substituted
    assert_eq!(
        result.resources[0].get_attr("availability_zone"),
        Some(&Value::String("ap-northeast-1a".to_string()))
    );
    assert_eq!(
        result.resources[1].get_attr("availability_zone"),
        Some(&Value::String("ap-northeast-1c".to_string()))
    );
}

#[test]
fn parse_top_level_if_expression() {
    let input = r#"
        let enabled = true
        if enabled {
            awscc.cloudwatch.alarm {
                alarm_name = "cpu-high"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("alarm_name"),
        Some(&Value::String("cpu-high".to_string()))
    );
}

#[test]
fn parse_top_level_multiple_for_no_collision() {
    let input = r#"
        for az in ["a", "b"] {
            awscc.ec2.Subnet {
                availability_zone = az
            }
        }
        for name in ["web", "api"] {
            awscc.ec2.SecurityGroup {
                group_name = name
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 4);

    // First for gets _for0, second gets _for1 - no collisions
    let names: Vec<&str> = result
        .resources
        .iter()
        .map(|r| r.id.name.as_str())
        .collect();
    assert_eq!(names[0], "_for0[0]");
    assert_eq!(names[1], "_for0[1]");
    assert_eq!(names[2], "_for1[0]");
    assert_eq!(names[3], "_for1[1]");
}

#[test]
fn parse_top_level_for_uses_iterable_name_as_binding() {
    let input = r#"
        let azs = ["ap-northeast-1a", "ap-northeast-1c"]
        for az in azs {
            awscc.ec2.Subnet {
                availability_zone = az
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 2);

    let names: Vec<&str> = result
        .resources
        .iter()
        .map(|r| r.id.name.as_str())
        .collect();
    assert_eq!(names[0], "_azs[0]");
    assert_eq!(names[1], "_azs[1]");
}

#[test]
fn parse_top_level_for_uses_last_segment_of_dotted_iterable() {
    let input = r#"
        let orgs = upstream_state {
            source = "../orgs"
        }
        for acct in orgs.accounts {
            awscc.sso.Assignment {
                target_id = acct
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    // Deferred (upstream_state not resolved), so no concrete resources
    // but the deferred_for_expressions should use _accounts
    assert_eq!(result.deferred_for_expressions[0].binding_name, "_accounts");
}

#[test]
fn parse_top_level_for_literal_list_uses_counter_fallback() {
    let input = r#"
        for az in ["a", "b"] {
            awscc.ec2.Subnet {
                availability_zone = az
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let names: Vec<&str> = result
        .resources
        .iter()
        .map(|r| r.id.name.as_str())
        .collect();
    assert_eq!(names[0], "_for0[0]");
    assert_eq!(names[1], "_for0[1]");
}

#[test]
fn parse_top_level_if_false_no_resources() {
    let input = r#"
        let enabled = false
        if enabled {
            awscc.cloudwatch.alarm {
                alarm_name = "cpu-high"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 0);
}

#[test]
fn parse_arguments_block_form_description_only() {
    let input = r#"
        arguments {
            vpc: awscc.ec2.Vpc {
                description = "The VPC to deploy into"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 1);
    assert_eq!(result.arguments[0].name, "vpc");
    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::Ref(ResourceTypePath::new("awscc", "ec2.Vpc"))
    );
    assert!(result.arguments[0].default.is_none());
    assert_eq!(
        result.arguments[0].description.as_deref(),
        Some("The VPC to deploy into")
    );
}

#[test]
fn parse_arguments_block_form_description_and_default() {
    let input = r#"
        arguments {
            port: Int {
                description = "Web server port"
                default     = 8080
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 1);
    assert_eq!(result.arguments[0].name, "port");
    assert_eq!(result.arguments[0].type_expr, TypeExpr::Int);
    assert_eq!(result.arguments[0].default, Some(Value::Int(8080)));
    assert_eq!(
        result.arguments[0].description.as_deref(),
        Some("Web server port")
    );
}

#[test]
fn parse_arguments_mixed_simple_and_block_form() {
    let input = r#"
        arguments {
            enable_https: Bool = true

            vpc: awscc.ec2.Vpc {
                description = "The VPC to deploy into"
            }

            port: Int {
                description = "Web server port"
                default     = 8080
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 3);

    // Simple form (unchanged)
    assert_eq!(result.arguments[0].name, "enable_https");
    assert_eq!(result.arguments[0].type_expr, TypeExpr::Bool);
    assert_eq!(result.arguments[0].default, Some(Value::Bool(true)));
    assert!(result.arguments[0].description.is_none());

    // Block form with description only
    assert_eq!(result.arguments[1].name, "vpc");
    assert_eq!(
        result.arguments[1].type_expr,
        TypeExpr::Ref(ResourceTypePath::new("awscc", "ec2.Vpc"))
    );
    assert!(result.arguments[1].default.is_none());
    assert_eq!(
        result.arguments[1].description.as_deref(),
        Some("The VPC to deploy into")
    );

    // Block form with description and default
    assert_eq!(result.arguments[2].name, "port");
    assert_eq!(result.arguments[2].type_expr, TypeExpr::Int);
    assert_eq!(result.arguments[2].default, Some(Value::Int(8080)));
    assert_eq!(
        result.arguments[2].description.as_deref(),
        Some("Web server port")
    );
}

#[test]
fn parse_arguments_simple_form_has_no_description() {
    let input = r#"
        arguments {
            vpc_id: String
            port: Int = 8080
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 2);
    assert!(result.arguments[0].description.is_none());
    assert!(result.arguments[1].description.is_none());
}

#[test]
fn parse_accepts_pascal_case_primitives() {
    let input = r#"
        arguments {
            a: String
            b: Int
            c: Bool
            d: Float
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
    assert_eq!(result.arguments[1].type_expr, TypeExpr::Int);
    assert_eq!(result.arguments[2].type_expr, TypeExpr::Bool);
    assert_eq!(result.arguments[3].type_expr, TypeExpr::Float);
}

#[test]
fn parse_still_accepts_lowercase_primitives_during_transition() {
    let input = r#"
        arguments {
            a: String
            b: Int
            c: Bool
            d: Float
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
    assert_eq!(result.arguments[1].type_expr, TypeExpr::Int);
    assert_eq!(result.arguments[2].type_expr, TypeExpr::Bool);
    assert_eq!(result.arguments[3].type_expr, TypeExpr::Float);
}

#[test]
fn parse_accepts_pascal_case_custom_types() {
    let input = r#"
        arguments {
            id: AwsAccountId
            cidr: Ipv4Cidr
            bucket_arn: Arn
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::Simple("aws_account_id".to_string())
    );
    assert_eq!(
        result.arguments[1].type_expr,
        TypeExpr::Simple("ipv4_cidr".to_string())
    );
    assert_eq!(
        result.arguments[2].type_expr,
        TypeExpr::Simple("arn".to_string())
    );
}

#[test]
fn parse_three_segment_resource_path_is_ref() {
    let input = r#"
        arguments {
            vpc: aws.ec2.Vpc
            bucket: aws.s3.Bucket
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    match &result.arguments[0].type_expr {
        TypeExpr::Ref(path) => {
            assert_eq!(path.provider, "aws");
            assert_eq!(path.resource_type, "ec2.Vpc");
        }
        other => panic!("expected Ref, got {other:?}"),
    }
    match &result.arguments[1].type_expr {
        TypeExpr::Ref(path) => {
            assert_eq!(path.provider, "aws");
            assert_eq!(path.resource_type, "s3.Bucket");
        }
        other => panic!("expected Ref, got {other:?}"),
    }
}

#[test]
fn parse_four_segment_path_with_pascal_tail_is_schema_type() {
    let input = r#"
        arguments {
            vpc_id: awscc.ec2.VpcId
        }
    "#;
    let mut ctx = ProviderContext::default();
    ctx.register_schema_type("awscc", "ec2", "VpcId");
    let result = parse(input, &ctx).unwrap();
    assert!(matches!(
        result.arguments[0].type_expr,
        TypeExpr::SchemaType { .. }
    ));
}

#[test]
fn type_expr_ref_display_roundtrips_three_segment_path() {
    let ty = TypeExpr::Ref(ResourceTypePath::new("aws", "ec2.Vpc"));
    assert_eq!(ty.to_string(), "aws.ec2.Vpc");

    let input = format!(r#"arguments {{ v: {} }}"#, ty);
    let parsed = parse(&input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.arguments[0].type_expr, ty);
}

#[test]
fn parser_rejects_lowercase_primitive_after_phase_c() {
    // Intentionally uses the old snake_case spelling to verify Phase C
    // rejection, so the type annotation below must NOT be mechanically
    // rewritten to PascalCase.
    let input = "arguments { a: string }";
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("unknown type 'string'") && msg.contains("'String'"),
        "expected rejection with hint pointing at 'String', got: {msg}"
    );
}

#[test]
fn parser_rejects_snake_case_custom_type_after_phase_c() {
    let input = "arguments { a: aws_account_id }";
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("unknown type 'aws_account_id'") && msg.contains("'AwsAccountId'"),
        "expected rejection with hint pointing at 'AwsAccountId', got: {msg}"
    );
}

#[test]
fn parser_does_not_warn_on_new_spelling() {
    let input = r#"
        arguments {
            a: String
            b: AwsAccountId
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert!(
        !result
            .warnings
            .iter()
            .any(|w| w.message.contains("deprecated type spelling")),
        "should not warn on new spellings, got {:?}",
        result.warnings
    );
}

#[test]
fn parse_arguments_block_form_default_only() {
    let input = r#"
        arguments {
            port: Int {
                default = 8080
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 1);
    assert_eq!(result.arguments[0].name, "port");
    assert_eq!(result.arguments[0].default, Some(Value::Int(8080)));
    assert!(result.arguments[0].description.is_none());
}

#[test]
fn parse_arguments_block_form_empty_block() {
    let input = r#"
        arguments {
            port: Int {}
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 1);
    assert_eq!(result.arguments[0].name, "port");
    assert!(result.arguments[0].default.is_none());
    assert!(result.arguments[0].description.is_none());
}

#[test]
fn parse_arguments_block_form_string_default_not_confused_with_description() {
    let input = r#"
        arguments {
            name: String {
                description = "Name of the resource"
                default     = "my-resource"
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 1);
    assert_eq!(result.arguments[0].name, "name");
    assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
    assert_eq!(
        result.arguments[0].description.as_deref(),
        Some("Name of the resource")
    );
    assert_eq!(
        result.arguments[0].default,
        Some(Value::String("my-resource".to_string()))
    );
}

#[test]
fn parse_arguments_block_form_validation_block() {
    let input = r#"
        arguments {
            port: Int {
                description = "Web server port"
                default     = 8080
                validation {
                    condition   = port >= 1 && port <= 65535
                    error_message = "Port must be between 1 and 65535"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 1);
    let arg = &result.arguments[0];
    assert_eq!(arg.name, "port");
    assert_eq!(arg.type_expr, TypeExpr::Int);
    assert_eq!(arg.default, Some(Value::Int(8080)));
    assert_eq!(arg.description.as_deref(), Some("Web server port"));
    assert_eq!(arg.validations.len(), 1);
    assert_eq!(
        arg.validations[0].error_message.as_deref(),
        Some("Port must be between 1 and 65535")
    );

    // Verify the validate expression structure:
    // port >= 1 && port <= 65535
    match &arg.validations[0].condition {
        ValidateExpr::And(left, right) => {
            match left.as_ref() {
                ValidateExpr::Compare { lhs, op, rhs } => {
                    assert_eq!(*lhs, Box::new(ValidateExpr::Var("port".to_string())));
                    assert_eq!(*op, CompareOp::Gte);
                    assert_eq!(*rhs, Box::new(ValidateExpr::Int(1)));
                }
                other => panic!("Expected Compare, got {:?}", other),
            }
            match right.as_ref() {
                ValidateExpr::Compare { lhs, op, rhs } => {
                    assert_eq!(*lhs, Box::new(ValidateExpr::Var("port".to_string())));
                    assert_eq!(*op, CompareOp::Lte);
                    assert_eq!(*rhs, Box::new(ValidateExpr::Int(65535)));
                }
                other => panic!("Expected Compare, got {:?}", other),
            }
        }
        other => panic!("Expected And, got {:?}", other),
    }
}

#[test]
fn parse_arguments_block_form_validate_no_description() {
    let input = r#"
        arguments {
            count: Int {
                validation {
                    condition = count > 0
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments.len(), 1);
    let arg = &result.arguments[0];
    assert_eq!(arg.validations.len(), 1);
    assert!(arg.validations[0].error_message.is_none());
    assert!(arg.description.is_none());
    assert!(arg.default.is_none());
}

#[test]
fn parse_arguments_block_form_validate_with_not() {
    let input = r#"
        arguments {
            enabled: Bool {
                validation {
                    condition = !enabled == false
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].validations.len(), 1);
}

#[test]
fn parse_arguments_block_form_validate_with_or() {
    let input = r#"
        arguments {
            port: Int {
                validation {
                    condition   = port == 80 || port == 443
                    error_message = "Port must be 80 or 443"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    match &result.arguments[0].validations[0].condition {
        ValidateExpr::Or(_, _) => {}
        other => panic!("Expected Or, got {:?}", other),
    }
}

#[test]
fn parse_arguments_block_form_validate_with_len() {
    let input = r#"
        arguments {
            name: String {
                validation {
                    condition   = len(name) >= 1 && len(name) <= 64
                    error_message = "Name must be between 1 and 64 characters"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].validations.len(), 1);
    assert_eq!(
        result.arguments[0].validations[0].error_message.as_deref(),
        Some("Name must be between 1 and 64 characters")
    );
}

#[test]
fn parse_arguments_block_form_multiple_validation_blocks() {
    let input = r#"
        arguments {
            port: Int {
                validation {
                    condition   = port >= 1
                    error_message = "Port must be positive"
                }
                validation {
                    condition   = port <= 65535
                    error_message = "Port must be at most 65535"
                }
            }
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].validations.len(), 2);
    assert_eq!(
        result.arguments[0].validations[0].error_message.as_deref(),
        Some("Port must be positive")
    );
    assert_eq!(
        result.arguments[0].validations[1].error_message.as_deref(),
        Some("Port must be at most 65535")
    );
}

#[test]
fn env_missing_var_produces_error_at_parse_time() {
    // Use a var name that is extremely unlikely to be set
    let input = r#"
        provider aws {
            region = aws.Region.ap_northeast_1
        }

        aws.s3.Bucket {
            name = env("CARINA_TEST_NONEXISTENT_VAR_12345")
        }
    "#;

    let result = parse_and_resolve(input);
    assert!(
        result.is_err(),
        "Expected error for missing env var, got: {:?}",
        result
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("CARINA_TEST_NONEXISTENT_VAR_12345"),
        "Error should mention the missing env var name, got: {}",
        err_msg
    );
}

#[test]
fn join_with_resolved_args_still_works() {
    let input = r#"
        provider aws {
            region = aws.Region.ap_northeast_1
        }

        aws.s3.Bucket {
            name = join("-", ["a", "b", "c"])
        }
    "#;

    let result = parse_and_resolve(input).unwrap();
    let resource = &result.resources[0];
    assert_eq!(
        resource.get_attr("name"),
        Some(&Value::String("a-b-c".to_string())),
    );
}

// --- User-defined function tests ---

#[test]
fn user_fn_simple_call() {
    let input = r#"
        fn greet(name) {
            join(" ", ["hello", name])
        }

        let vpc = aws.s3_bucket {
            name = greet("world")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("hello world".to_string())),
    );
}

#[test]
fn user_fn_with_default_param() {
    let input = r#"
        fn tag(env, suffix = "default") {
            join("-", [env, suffix])
        }

        let a = aws.s3_bucket {
            name = tag("prod")
        }

        let b = aws.s3_bucket {
            name = tag("prod", "web")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("prod-default".to_string())),
    );
    assert_eq!(
        result.resources[1].get_attr("name"),
        Some(&Value::String("prod-web".to_string())),
    );
}

#[test]
fn user_fn_with_local_let() {
    let input = r#"
        fn subnet_name(env, az) {
            let prefix = join("-", [env, "subnet"])
            join("-", [prefix, az])
        }

        let vpc = aws.s3_bucket {
            name = subnet_name("prod", "a")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("prod-subnet-a".to_string())),
    );
}

#[test]
fn user_fn_calling_builtin() {
    let input = r#"
        fn upper_name(name) {
            upper(name)
        }

        let vpc = aws.s3_bucket {
            name = upper_name("hello")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("HELLO".to_string())),
    );
}

#[test]
fn user_fn_calling_another_fn() {
    let input = r#"
        fn prefix(env) {
            join("-", [env, "app"])
        }

        fn full_name(env, service) {
            join("-", [prefix(env), service])
        }

        let vpc = aws.s3_bucket {
            name = full_name("prod", "web")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("prod-app-web".to_string())),
    );
}

#[test]
fn user_fn_recursive_call_errors() {
    let input = r#"
        fn recurse(x) {
            recurse(x)
        }

        let vpc = aws.s3_bucket {
            name = recurse("hello")
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Recursive function call"),
        "Expected recursive function error, got: {err}"
    );
}

#[test]
fn user_fn_missing_required_arg_errors() {
    let input = r#"
        fn greet(name, title) {
            join(" ", [title, name])
        }

        let vpc = aws.s3_bucket {
            name = greet("world")
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("expects at least 2"),
        "Expected missing arg error, got: {err}"
    );
}

#[test]
fn user_fn_too_many_args_errors() {
    let input = r#"
        fn greet(name) {
            join(" ", ["hello", name])
        }

        let vpc = aws.s3_bucket {
            name = greet("world", "extra")
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("expects at most 1"),
        "Expected too many args error, got: {err}"
    );
}

#[test]
fn user_fn_shadows_builtin_errors() {
    let input = r#"
        fn join(sep, items) {
            sep
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("shadows a built-in function"),
        "Expected shadow error, got: {err}"
    );
}

#[test]
fn user_fn_duplicate_definition_errors() {
    let input = r#"
        fn greet(name) {
            name
        }

        fn greet(x) {
            x
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("duplicate function definition"),
        "Expected duplicate error, got: {err}"
    );
}

#[test]
fn user_fn_stored_in_parsed_file() {
    let input = r#"
        fn greet(name) {
            join(" ", ["hello", name])
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert!(result.user_functions.contains_key("greet"));
    let func = &result.user_functions["greet"];
    assert_eq!(func.name, "greet");
    assert_eq!(func.params.len(), 1);
    assert_eq!(func.params[0].name, "name");
}

#[test]
fn user_fn_no_params() {
    let input = r#"
        fn hello() {
            "hello"
        }

        let vpc = aws.s3_bucket {
            name = hello()
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("hello".to_string())),
    );
}

#[test]
fn user_fn_indirect_recursion_errors() {
    let input = r#"
        fn foo(x) {
            bar(x)
        }

        fn bar(x) {
            foo(x)
        }

        let vpc = aws.s3_bucket {
            name = foo("hello")
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Recursive function call"),
        "Expected recursive function error, got: {err}"
    );
}

#[test]
fn user_fn_required_param_after_optional_errors() {
    let input = r#"
        fn bad(a = "x", b) {
            join("-", [a, b])
        }
    "#;

    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("required parameter") && err.contains("cannot follow optional"),
        "Expected param ordering error, got: {err}"
    );
}

#[test]
fn user_fn_with_pipe_operator() {
    let input = r#"
        fn wrap(prefix, val) {
            join("-", [prefix, val])
        }

        let vpc = aws.s3_bucket {
            name = "world" |> wrap("hello")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("hello-world".to_string())),
    );
}

#[test]
fn user_fn_with_string_interpolation() {
    let input = r#"
        fn greet(name) {
            join(" ", ["hello", name])
        }

        let vpc = aws.s3_bucket {
            name = "${greet("world")}-suffix"
        }
    "#;

    // The greet() call evaluates to "hello world", which folds into
    // the surrounding "-suffix" literal.
    let result = parse(input, &ProviderContext::default()).unwrap();
    let name = result.resources[0].get_attr("name").unwrap();
    assert_eq!(name, &Value::String("hello world-suffix".to_string()));
}

#[test]
fn user_fn_typed_param_string() {
    let input = r#"
        fn greet(name: String) {
            join(" ", ["hello", name])
        }

        let vpc = aws.s3_bucket {
            name = greet("world")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("hello world".to_string())),
    );
}

#[test]
fn user_fn_typed_param_type_mismatch() {
    let input = r#"
        fn greet(name: String) {
            name
        }

        let vpc = aws.s3_bucket {
            name = greet(42)
        }
    "#;

    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expects type 'String'"),
        "Expected type mismatch error, got: {msg}"
    );
}

#[test]
fn user_fn_typed_param_int() {
    let input = r#"
        fn double(x: Int) {
            x
        }

        let vpc = aws.s3_bucket {
            name = double("not_int")
        }
    "#;

    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expects type 'Int'"),
        "Expected type mismatch error, got: {msg}"
    );
}

#[test]
fn user_fn_typed_param_with_default() {
    let input = r#"
        fn tag(env: String, suffix: String = "default") {
            join("-", [env, suffix])
        }

        let a = aws.s3_bucket {
            name = tag("prod")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("prod-default".to_string())),
    );
}

#[test]
fn user_fn_mixed_typed_and_untyped() {
    let input = r#"
        fn tag(env, suffix: String) {
            join("-", [env, suffix])
        }

        let a = aws.s3_bucket {
            name = tag("prod", "web")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.resources[0].get_attr("name"),
        Some(&Value::String("prod-web".to_string())),
    );
}

#[test]
fn user_fn_typed_param_bool_mismatch() {
    let input = r#"
        fn check(flag: Bool) {
            flag
        }

        let vpc = aws.s3_bucket {
            name = check("not_bool")
        }
    "#;

    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expects type 'Bool'"),
        "Expected type mismatch error, got: {msg}"
    );
}

#[test]
fn user_fn_param_type_stored_in_parsed_file() {
    let input = r#"
        fn greet(name: String, count: Int) {
            name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("greet").unwrap();
    assert_eq!(func.params[0].param_type, Some(TypeExpr::String));
    assert_eq!(func.params[1].param_type, Some(TypeExpr::Int));
}

#[test]
fn user_fn_untyped_param_type_is_none() {
    let input = r#"
        fn greet(name) {
            name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("greet").unwrap();
    assert_eq!(func.params[0].param_type, None);
}

#[test]
fn user_fn_return_type_string() {
    let input = r#"
        fn greet(name: String): String {
            name
        }

        let vpc = aws.s3_bucket {
            name = greet("hello")
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("greet").unwrap();
    assert_eq!(func.return_type, Some(TypeExpr::String));
}

#[test]
fn user_fn_return_type_none_when_omitted() {
    let input = r#"
        fn greet(name) {
            name
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("greet").unwrap();
    assert_eq!(func.return_type, None);
}

#[test]
fn user_fn_return_type_mismatch_value() {
    let input = r#"
        fn bad(): String {
            42
        }

        let vpc = aws.s3_bucket {
            name = bad()
        }
    "#;

    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("return type"),
        "Expected return type error, got: {msg}"
    );
}

#[test]
fn parse_custom_schema_type_in_fn_param() {
    // Custom schema types like ipv4_cidr, ipv4_address, arn should be accepted as type annotations
    let input = r#"
        fn format_cidr(cidr_block: Ipv4Cidr) {
            cidr_block
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("format_cidr").unwrap();
    assert_eq!(func.params[0].name, "cidr_block");
    assert_eq!(
        func.params[0].param_type,
        Some(TypeExpr::Simple("ipv4_cidr".to_string()))
    );
}

#[test]
fn parse_ipv4_address_type_in_fn_param() {
    let input = r#"
        fn f(addr: Ipv4Address) {
            addr
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("f").unwrap();
    assert_eq!(
        func.params[0].param_type,
        Some(TypeExpr::Simple("ipv4_address".to_string()))
    );
}

#[test]
fn parse_arn_type_in_fn_param() {
    let input = r#"
        fn f(role: Arn) {
            role
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("f").unwrap();
    assert_eq!(
        func.params[0].param_type,
        Some(TypeExpr::Simple("arn".to_string()))
    );
}

#[test]
fn parse_custom_type_in_list_generic() {
    let input = r#"
        fn f(cidrs: list(Ipv4Cidr)) {
            cidrs
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let func = result.user_functions.get("f").unwrap();
    assert_eq!(
        func.params[0].param_type,
        Some(TypeExpr::List(Box::new(TypeExpr::Simple(
            "ipv4_cidr".to_string()
        ))))
    );
}

#[test]
fn parse_custom_type_in_module_arguments() {
    let input = r#"
        arguments {
            vpc_cidr: Ipv4Cidr
            server_ip: Ipv4Address
        }

        awscc.ec2.Vpc {
            name       = "test"
            cidr_block = vpc_cidr
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.arguments[0].name, "vpc_cidr");
    assert_eq!(
        result.arguments[0].type_expr,
        TypeExpr::Simple("ipv4_cidr".to_string())
    );
    assert_eq!(result.arguments[1].name, "server_ip");
    assert_eq!(
        result.arguments[1].type_expr,
        TypeExpr::Simple("ipv4_address".to_string())
    );
}

#[test]
fn parse_custom_type_in_attributes() {
    let input = r#"
        attributes {
            block: Ipv4Cidr = vpc.cidr_block
        }

        let vpc = awscc.ec2.Vpc {
            name       = "test"
            cidr_block = "10.0.0.0/16"
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.attribute_params[0].type_expr,
        Some(TypeExpr::Simple("ipv4_cidr".to_string()))
    );
}

#[test]
fn type_expr_display_simple() {
    assert_eq!(
        TypeExpr::Simple("ipv4_cidr".to_string()).to_string(),
        "Ipv4Cidr"
    );
    assert_eq!(
        TypeExpr::Simple("ipv4_address".to_string()).to_string(),
        "Ipv4Address"
    );
    assert_eq!(TypeExpr::Simple("arn".to_string()).to_string(), "Arn");
}

#[test]
fn type_expr_display_simple_is_pascal_case() {
    assert_eq!(
        TypeExpr::Simple("aws_account_id".to_string()).to_string(),
        "AwsAccountId"
    );
    assert_eq!(
        TypeExpr::Simple("ipv4_cidr".to_string()).to_string(),
        "Ipv4Cidr"
    );
    assert_eq!(TypeExpr::Simple("arn".to_string()).to_string(), "Arn");
}

// --- Issue #1285: Validate fn call arguments for custom types ---

#[test]
fn user_fn_custom_type_cidr_arg_valid() {
    let input = r#"
        fn f(x: Ipv4Cidr) { x }

        let b = aws.s3_bucket {
            name = f("10.0.0.0/16")
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
}

#[test]
fn user_fn_custom_type_cidr_arg_invalid() {
    let input = r#"
        fn f(x: Ipv4Cidr) { x }

        let b = aws.s3_bucket {
            name = f("invalid")
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("type 'ipv4_cidr' validation failed"),
        "Expected ipv4_cidr validation error, got: {msg}"
    );
}

#[test]
fn user_fn_custom_type_ipv4_address_arg_valid() {
    let input = r#"
        fn f(x: Ipv4Address) { x }

        let b = aws.s3_bucket {
            name = f("10.0.0.1")
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
}

#[test]
fn user_fn_custom_type_ipv4_address_arg_invalid() {
    let input = r#"
        fn f(x: Ipv4Address) { x }

        let b = aws.s3_bucket {
            name = f("invalid")
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("type 'ipv4_address' validation failed"),
        "Expected ipv4_address validation error, got: {msg}"
    );
}

#[test]
fn user_fn_custom_type_ipv6_cidr_arg_valid() {
    let input = r#"
        fn f(x: Ipv6Cidr) { x }

        let b = aws.s3_bucket {
            name = f("2001:db8::/32")
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
}

#[test]
fn user_fn_custom_type_ipv6_cidr_arg_invalid() {
    let input = r#"
        fn f(x: Ipv6Cidr) { x }

        let b = aws.s3_bucket {
            name = f("invalid")
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("type 'ipv6_cidr' validation failed"),
        "Expected ipv6_cidr validation error, got: {msg}"
    );
}

#[test]
fn user_fn_custom_type_ipv6_address_arg_valid() {
    let input = r#"
        fn f(x: Ipv6Address) { x }

        let b = aws.s3_bucket {
            name = f("2001:db8::1")
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
}

#[test]
fn user_fn_custom_type_ipv6_address_arg_invalid() {
    let input = r#"
        fn f(x: Ipv6Address) { x }

        let b = aws.s3_bucket {
            name = f("invalid")
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("type 'ipv6_address' validation failed"),
        "Expected ipv6_address validation error, got: {msg}"
    );
}

#[test]
fn user_fn_custom_type_arn_arg_accepts_string() {
    // arn format varies too much, just accept any string
    let input = r#"
        fn f(x: Arn) { x }

        let b = aws.s3_bucket {
            name = f("arn:aws:s3:::my-bucket")
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
}

#[test]
fn user_fn_custom_type_arg_resource_ref_skipped() {
    // ResourceRef values should be accepted (resolved later)
    let input = r#"
        fn f(x: Ipv4Cidr) { x }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }

        let b = aws.s3_bucket {
            name = f(vpc.cidr_block)
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
}

// --- Issue #1284: Validate fn return type for custom types ---

#[test]
fn user_fn_custom_type_return_cidr_valid() {
    let input = r#"
        fn f(): Ipv4Cidr { "10.0.0.0/16" }

        let b = aws.s3_bucket {
            name = f()
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
}

#[test]
fn user_fn_custom_type_return_cidr_invalid() {
    let input = r#"
        fn f(): Ipv4Cidr { "invalid" }

        let b = aws.s3_bucket {
            name = f()
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("return type 'ipv4_cidr' validation failed"),
        "Expected ipv4_cidr validation error, got: {msg}"
    );
}

#[test]
fn user_fn_custom_type_return_ipv4_address_invalid() {
    let input = r#"
        fn f(): Ipv4Address { "invalid" }

        let b = aws.s3_bucket {
            name = f()
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("return type 'ipv4_address' validation failed"),
        "Expected ipv4_address validation error, got: {msg}"
    );
}

#[test]
fn user_fn_custom_type_return_ipv6_cidr_invalid() {
    let input = r#"
        fn f(): Ipv6Cidr { "invalid" }

        let b = aws.s3_bucket {
            name = f()
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("return type 'ipv6_cidr' validation failed"),
        "Expected ipv6_cidr validation error, got: {msg}"
    );
}

#[test]
fn user_fn_custom_type_return_ipv6_address_invalid() {
    let input = r#"
        fn f(): Ipv6Address { "invalid" }

        let b = aws.s3_bucket {
            name = f()
        }
    "#;
    let err = parse(input, &ProviderContext::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("return type 'ipv6_address' validation failed"),
        "Expected ipv6_address validation error, got: {msg}"
    );
}

// --- ProviderContext tests ---

#[test]
fn parse_decrypt_uses_config_decryptor() {
    use std::collections::HashMap;
    let config = ProviderContext {
        decryptor: Some(Box::new(|ciphertext, _key| {
            Ok(format!("decrypted:{ciphertext}"))
        })),
        validators: HashMap::new(),
        custom_type_validator: None,
        schema_types: Default::default(),
    };

    // decrypt() in resource attributes is resolved during resolve_resource_refs,
    // so we need to parse and then resolve with config.
    let input = r#"
        let my_bucket = aws.s3_bucket {
            name   = "test-bucket"
            secret = decrypt("AQICAHh")
        }
    "#;
    let mut parsed = parse(input, &config).unwrap();
    resolve_resource_refs_with_config(&mut parsed, &config).unwrap();
    assert_eq!(parsed.resources.len(), 1); // allow: direct — fixture test inspection
    let secret_val = parsed.resources[0].get_attr("secret").unwrap();
    assert_eq!(*secret_val, Value::String("decrypted:AQICAHh".to_string()));
}

#[test]
fn parse_decrypt_without_decryptor_errors() {
    let config = ProviderContext::default();

    let input = r#"
        let my_bucket = aws.s3_bucket {
            name   = "test-bucket"
            secret = decrypt("AQICAHh")
        }
    "#;
    let mut parsed = parse(input, &config).unwrap();
    let result = resolve_resource_refs_with_config(&mut parsed, &config);
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("requires a configured provider"),
        "Expected decryptor error, got: {msg}"
    );
}

#[test]
fn parse_custom_validator_accepts_valid() {
    use std::collections::HashMap;
    // Test validate_custom_type directly with a type name that has no built-in
    // handler. Built-in types (cidr, ipv4_address, etc.) are matched first in
    // validate_custom_type, so custom validators only apply to other type names.
    let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
    validators.insert(
        "custom_type".to_string(),
        Box::new(|s: &str| {
            if s.starts_with("valid-") {
                Ok(())
            } else {
                Err(format!("custom_type must start with 'valid-', got '{s}'"))
            }
        }),
    );
    let config = ProviderContext {
        decryptor: None,
        validators,
        custom_type_validator: None,
        schema_types: Default::default(),
    };

    let result = validate_custom_type(
        "custom_type",
        &Value::String("valid-data".to_string()),
        &config,
    );
    assert!(result.is_ok());

    // Unknown type with no custom validator should also pass (permissive)
    let result = validate_custom_type(
        "unknown_type",
        &Value::String("anything".to_string()),
        &config,
    );
    assert!(result.is_ok());
}

#[test]
fn parse_custom_validator_rejects_invalid() {
    use std::collections::HashMap;
    // Use a type name that the grammar accepts and has no built-in validator.
    // The "arn" type is accepted by the grammar as identifier. But it fails to parse.
    // Use "cidr" which is known to work in grammar. Register a custom stricter validator.
    // Actually, let's test validate_custom_type directly to avoid grammar issues.
    let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
    validators.insert(
        "custom_type".to_string(),
        Box::new(|s: &str| {
            if s.starts_with("valid-") {
                Ok(())
            } else {
                Err(format!("custom_type must start with 'valid-', got '{s}'"))
            }
        }),
    );
    let config = ProviderContext {
        decryptor: None,
        validators,
        custom_type_validator: None,
        schema_types: Default::default(),
    };

    // Test validate_custom_type directly since the grammar may not accept
    // arbitrary type names. This verifies the custom validator is called.
    let valid_result = validate_custom_type(
        "custom_type",
        &Value::String("valid-data".to_string()),
        &config,
    );
    assert!(valid_result.is_ok());

    let invalid_result = validate_custom_type(
        "custom_type",
        &Value::String("invalid".to_string()),
        &config,
    );
    assert!(invalid_result.is_err());
    let msg = invalid_result.unwrap_err();
    assert!(
        msg.contains("custom_type must start with 'valid-'"),
        "Expected validation error, got: {msg}"
    );
}

#[test]
fn pascal_to_snake_conversion() {
    assert_eq!(super::pascal_to_snake("VpcId"), "vpc_id");
    assert_eq!(super::pascal_to_snake("SubnetId"), "subnet_id");
    assert_eq!(
        super::pascal_to_snake("SecurityGroupId"),
        "security_group_id"
    );
    assert_eq!(super::pascal_to_snake("Arn"), "arn");
    assert_eq!(super::pascal_to_snake("IamRoleArn"), "iam_role_arn");
}

#[test]
fn snake_to_pascal_conversion() {
    use super::snake_to_pascal;
    assert_eq!(snake_to_pascal("vpc_id"), "VpcId");
    assert_eq!(snake_to_pascal("aws_account_id"), "AwsAccountId");
    assert_eq!(snake_to_pascal("iam_policy_arn"), "IamPolicyArn");
    assert_eq!(snake_to_pascal("ipv4_cidr"), "Ipv4Cidr");
    assert_eq!(snake_to_pascal("arn"), "Arn");
    assert_eq!(snake_to_pascal("kms_key_arn"), "KmsKeyArn");
    for name in [
        "vpc_id",
        "aws_account_id",
        "iam_policy_arn",
        "ipv4_cidr",
        "arn",
    ] {
        assert_eq!(pascal_to_snake(&snake_to_pascal(name)), name);
    }
}

#[test]
fn parse_schema_type_in_arguments() {
    let input = r#"
arguments {
  vpc_id: awscc.ec2.VpcId
}
"#;
    let mut ctx = ProviderContext::default();
    ctx.register_schema_type("awscc", "ec2", "VpcId");
    let parsed = parse(input, &ctx).unwrap();
    assert_eq!(parsed.arguments.len(), 1);
    let arg = &parsed.arguments[0];
    assert_eq!(arg.name, "vpc_id");
    match &arg.type_expr {
        TypeExpr::SchemaType {
            provider,
            path,
            type_name,
        } => {
            assert_eq!(provider, "awscc");
            assert_eq!(path, "ec2");
            assert_eq!(type_name, "VpcId");
        }
        other => panic!("Expected SchemaType, got {:?}", other),
    }
}

#[test]
fn parse_schema_type_display() {
    let t = TypeExpr::SchemaType {
        provider: "awscc".to_string(),
        path: "ec2".to_string(),
        type_name: "VpcId".to_string(),
    };
    assert_eq!(t.to_string(), "awscc.ec2.VpcId");
}

#[test]
fn parse_schema_type_list() {
    let input = r#"
arguments {
  subnet_ids: list(awscc.ec2.SubnetId)
}
"#;
    let mut ctx = ProviderContext::default();
    ctx.register_schema_type("awscc", "ec2", "SubnetId");
    let parsed = parse(input, &ctx).unwrap();
    assert_eq!(parsed.arguments.len(), 1);
    let arg = &parsed.arguments[0];
    match &arg.type_expr {
        TypeExpr::List(inner) => match inner.as_ref() {
            TypeExpr::SchemaType { type_name, .. } => {
                assert_eq!(type_name, "SubnetId");
            }
            other => panic!("Expected SchemaType inside list, got {:?}", other),
        },
        other => panic!("Expected List, got {:?}", other),
    }
}

#[test]
fn parse_let_discard_read_resource() {
    let input = r#"
        provider aws {
            region = aws.Region.ap_northeast_1
        }

        let _ = read aws.sts.caller_identity {}
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id.resource_type, "sts.caller_identity");
    assert_eq!(
        result.resources[0].kind,
        crate::resource::ResourceKind::DataSource
    );
}

#[test]
fn parse_upstream_state_registers_binding() {
    // After parsing upstream_state, the binding should be registered so that
    // `network.vpc.vpc_id` is parsed as a ResourceRef.
    let input = r#"
        let network = upstream_state {
            source = "../network"
        }

        let web_sg = awscc.ec2.SecurityGroup {
            name = "web-sg"
            vpc_id = network.vpc.vpc_id
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.upstream_states.len(), 1);
    assert_eq!(result.resources.len(), 1);
    let vpc_id_attr = result.resources[0].get_attr("vpc_id").unwrap();
    match vpc_id_attr {
        Value::ResourceRef { path } => {
            assert_eq!(path.binding(), "network");
            assert_eq!(path.attribute(), "vpc");
            assert_eq!(path.field_path(), vec!["vpc_id"]);
        }
        other => panic!("Expected ResourceRef, got: {:?}", other),
    }
}

#[test]
fn test_parse_require_statement() {
    let input = r#"
        arguments {
            enable_https: Bool = true
            has_cert: Bool = false
        }
        require !enable_https || has_cert, "cert is required when HTTPS is enabled"
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.requires.len(), 1);
    assert_eq!(
        result.requires[0].error_message,
        "cert is required when HTTPS is enabled"
    );
    // Verify the condition is an Or expression
    match &result.requires[0].condition {
        ValidateExpr::Or(_, _) => {}
        other => panic!("Expected Or expression, got {:?}", other),
    }
}

#[test]
fn test_parse_require_with_len_function() {
    let input = r#"
        arguments {
            subnet_ids: list(String)
        }
        require len(subnet_ids) >= 2, "ALB requires at least two subnets"
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.requires.len(), 1);
    assert_eq!(
        result.requires[0].error_message,
        "ALB requires at least two subnets"
    );
    match &result.requires[0].condition {
        ValidateExpr::Compare { lhs, op, rhs } => {
            assert!(
                matches!(lhs.as_ref(), ValidateExpr::FunctionCall { name, .. } if name == "len")
            );
            assert_eq!(*op, CompareOp::Gte);
            assert_eq!(**rhs, ValidateExpr::Int(2));
        }
        other => panic!("Expected Compare expression, got {:?}", other),
    }
}

#[test]
fn test_parse_require_with_null() {
    let input = r#"
        arguments {
            cert_arn: String = "default"
        }
        require cert_arn != null, "cert_arn must not be null"
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.requires.len(), 1);
    match &result.requires[0].condition {
        ValidateExpr::Compare { lhs, op, rhs } => {
            assert!(matches!(lhs.as_ref(), ValidateExpr::Var(name) if name == "cert_arn"));
            assert_eq!(*op, CompareOp::Ne);
            assert_eq!(**rhs, ValidateExpr::Null);
        }
        other => panic!("Expected Compare expression, got {:?}", other),
    }
}

#[test]
fn test_parse_multiple_require_statements() {
    let input = r#"
        arguments {
            min_size: Int
            max_size: Int
            subnet_ids: list(String)
        }
        require min_size <= max_size, "min_size must be <= max_size"
        require len(subnet_ids) >= 2, "need at least two subnets"
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.requires.len(), 2);
    assert_eq!(
        result.requires[0].error_message,
        "min_size must be <= max_size"
    );
    assert_eq!(
        result.requires[1].error_message,
        "need at least two subnets"
    );
}

#[test]
fn test_parse_require_with_and_operator() {
    let input = r#"
        arguments {
            port: Int = 80
        }
        require port >= 1 && port <= 65535, "port must be between 1 and 65535"
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.requires.len(), 1);
    match &result.requires[0].condition {
        ValidateExpr::And(_, _) => {}
        other => panic!("Expected And expression, got {:?}", other),
    }
}

#[test]
fn test_parse_require_null_prefixed_variable() {
    // Ensure variables with names starting with "null" (e.g., "nullable")
    // are not mis-parsed as null_literal
    let input = r#"
        arguments {
            nullable: Bool = true
        }
        require nullable, "must be true"
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(result.requires.len(), 1);
    match &result.requires[0].condition {
        ValidateExpr::Var(name) => {
            assert_eq!(name, "nullable");
        }
        other => panic!("Expected Var('nullable'), got {:?}", other),
    }
}

#[test]
fn test_compose_operator_followed_by_pipe_consumes_closure() {
    // After #2230, the composed closure produced by `>>` lives on
    // `EvalValue` and is consumed by the later pipe. The
    // intermediate binding `f` is an evaluator artifact and is
    // dropped at the parse boundary; only the fully-reduced
    // `result` survives.
    let input = r#"
        let f = map(".id") >> join(",")
        let result = [{ id = "a" }, { id = "b" }] |> f()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();

    assert_eq!(
        result.variables.get("result").unwrap(),
        &Value::String("a,b".to_string())
    );
    // `f` is a closure-only binding and does not appear in the
    // user-facing variable map.
    assert!(result.variables.get("f").is_none());
}

#[test]
fn test_compose_operator_with_pipe() {
    // Compose then use via pipe
    let input = r#"
        let transform = map(".name") >> join(", ")
        let names = [{ name = "alice" }, { name = "bob" }] |> transform()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("names").unwrap(),
        &Value::String("alice, bob".to_string())
    );
}

#[test]
fn test_compose_operator_two_step_chain() {
    // split(",") >> join("-") composed and applied
    let input = r#"
        let transform = split(",") >> join("-")
        let result = "a,b,c" |> transform()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("result").unwrap(),
        &Value::String("a-b-c".to_string())
    );
}

#[test]
fn test_compose_operator_error_on_non_closure_lhs() {
    // "hello" >> join(",") should fail
    let input = r#"
        let f = "hello" >> join(",")
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("left side of >> must be a Closure"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_compose_operator_error_on_non_closure_rhs() {
    // join(",") >> "hello" should fail
    let input = r#"
        let f = join(",") >> "hello"
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("right side of >> must be a Closure"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_compose_operator_precedence_with_pipe() {
    // Compose used with pipe via variable
    let input = r#"
        let pipeline = map(".x") >> join("-")
        let data = [{ x = "1" }, { x = "2" }]
        let result = data |> pipeline()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("result").unwrap(),
        &Value::String("1-2".to_string())
    );
}

#[test]
fn test_compose_three_functions() {
    // Three-way composition: parser must accept the chain and
    // (via #2230) keep the result confined to the evaluator-only
    // `EvalValue` layer. The binding is dropped from the
    // user-facing variable map; the test that the chain still
    // *applies* correctly is covered by
    // `test_compose_operator_followed_by_pipe_consumes_closure`.
    let input = r#"
        let transform = split(",") >> join("-") >> split("-")
    "#;
    let result =
        parse(input, &ProviderContext::default()).expect("three-way composition should parse");
    assert!(result.variables.get("transform").is_none());
}

#[test]
fn parse_single_quoted_string_literal() {
    let input = r#"
        let vpc = aws.ec2.Vpc {
            name = 'my-vpc'
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("my-vpc".to_string()))
    );
}

#[test]
fn parse_single_quoted_string_no_interpolation() {
    // Single-quoted strings should NOT support interpolation — ${...} is literal
    let input = r#"
        let env = "prod"
        let vpc = aws.ec2.Vpc {
            name = 'vpc-${env}'
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    // Should be a plain string, not interpolated
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("vpc-${env}".to_string()))
    );
}

#[test]
fn parse_single_quoted_string_escape_sequences() {
    let input = r#"
        let vpc = aws.ec2.Vpc {
            name = 'it\'s a test'
        }
    "#;

    let result = parse(input, &ProviderContext::default()).unwrap();
    let vpc = &result.resources[0];
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::String("it's a test".to_string()))
    );
}

#[test]
fn test_compose_three_functions_execution() {
    // Three-way composition applied end-to-end:
    // split(",") >> join("-") >> split("-") — split, rejoin, then split again
    let input = r#"
        let transform = split(",") >> join("-") >> split("-")
        let result = "a,b,c" |> transform()
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("result").unwrap(),
        &Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
            Value::String("c".to_string()),
        ])
    );
}

#[test]
fn parse_heredoc_basic() {
    let input = r#"
        aws.iam.Role {
            name = "my-role"
            policy = <<EOT
{
  "Version": "2012-10-17"
}
EOT
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let resource = &result.resources[0];
    assert_eq!(
        resource.get_attr("policy"),
        Some(&Value::String(
            "{\n  \"Version\": \"2012-10-17\"\n}".to_string()
        ))
    );
}

#[test]
fn parse_heredoc_indented() {
    // <<- strips common leading whitespace
    let input = "aws.iam.Role {\n    name = \"my-role\"\n    policy = <<-EOT\n        line1\n        line2\n        line3\n    EOT\n}\n";
    let result = parse(input, &ProviderContext::default()).unwrap();
    let resource = &result.resources[0];
    assert_eq!(
        resource.get_attr("policy"),
        Some(&Value::String("line1\nline2\nline3".to_string()))
    );
}

#[test]
fn parse_heredoc_empty() {
    let input = "aws.iam.Role {\n    name = \"my-role\"\n    policy = <<EOT\nEOT\n}\n";
    let result = parse(input, &ProviderContext::default()).unwrap();
    let resource = &result.resources[0];
    assert_eq!(
        resource.get_attr("policy"),
        Some(&Value::String("".to_string()))
    );
}

#[test]
fn parse_heredoc_in_let_binding() {
    let input = r#"
        let doc = <<EOF
hello world
EOF
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(
        result.variables.get("doc"),
        Some(&Value::String("hello world".to_string()))
    );
}

#[test]
fn quoted_string_as_map_key() {
    let input = r#"
        let m = {
            'token.actions.githubusercontent.com:aud' = 'sts.amazonaws.com'
            "aws:SourceIp" = '10.0.0.0/8'
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    if let Some(Value::Map(map)) = result.variables.get("m") {
        assert_eq!(
            map.get("token.actions.githubusercontent.com:aud"),
            Some(&Value::String("sts.amazonaws.com".to_string()))
        );
        assert_eq!(
            map.get("aws:SourceIp"),
            Some(&Value::String("10.0.0.0/8".to_string()))
        );
    } else {
        panic!("Expected map, got {:?}", result.variables.get("m"));
    }
}

#[test]
fn quoted_string_as_attribute_key_in_block() {
    let input = r#"
        awscc.iam.role {
            name = 'test-role'
            assume_role_policy_document = {
                version = '2012-10-17'
                statement {
                    effect = 'Allow'
                    action = 'sts:AssumeRoleWithWebIdentity'
                    condition = {
                        string_equals = {
                            'token.actions.githubusercontent.com:aud' = 'sts.amazonaws.com'
                        }
                    }
                }
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default()).unwrap();
    let resource = &result.resources[0];
    // Navigate: assume_role_policy_document -> statement[0] -> condition -> string_equals
    let doc = resource.get_attr("assume_role_policy_document").unwrap();
    if let Value::Map(doc_map) = doc
        && let Some(Value::List(statements)) = doc_map.get("statement")
        && let Value::Map(stmt) = &statements[0]
        && let Some(Value::Map(condition)) = stmt.get("condition")
        && let Some(Value::Map(string_equals)) = condition.get("string_equals")
    {
        assert_eq!(
            string_equals.get("token.actions.githubusercontent.com:aud"),
            Some(&Value::String("sts.amazonaws.com".to_string()))
        );
    } else {
        panic!("Could not navigate to condition key");
    }
}

#[test]
fn parse_exports_block_basic() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_id = vpc.vpc_id
}
"#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.export_params.len(), 1);
    assert_eq!(parsed.export_params[0].name, "vpc_id");
    assert!(parsed.export_params[0].type_expr.is_none());
    assert!(parsed.export_params[0].value.is_some());
}

#[test]
fn parse_exports_block_with_type() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_id: String = vpc.vpc_id
  cidr: String = vpc.cidr_block
}
"#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.export_params.len(), 2);
    assert_eq!(parsed.export_params[0].name, "vpc_id");
    assert!(parsed.export_params[0].type_expr.is_some());
    assert_eq!(parsed.export_params[1].name, "cidr");
}

#[test]
fn parse_exports_block_list_round_trips_through_formatter() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_ids: list(String) = [
vpc.vpc_id,
  ]
}
"#;

    let original = parse(input, &ProviderContext::default()).unwrap();
    let formatted =
        crate::formatter::format(input, &crate::formatter::FormatConfig::default()).unwrap();
    let reparsed = parse(&formatted, &ProviderContext::default()).unwrap();

    assert_eq!(
        formatted,
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_ids: list(String) = [vpc.vpc_id]
}
"#
    );
    assert_eq!(original.export_params, reparsed.export_params);
}

#[test]
fn coalesce_operator_returns_default_for_unresolved_ref() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

awscc.ec2.Subnet {
  cidr_block = vpc.missing_attr ?? '10.0.1.0/24'
}
"#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    // vpc.missing_attr is a ResourceRef (unresolved at parse time), so ?? returns default
    let subnet = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "ec2.Subnet")
        .unwrap();
    let cidr = subnet.get_attr("cidr_block");
    // At parse time, vpc.missing_attr is still a ResourceRef (not resolved), so ?? kicks in
    // Actually, resource refs remain as ResourceRef until resolution, so the left side IS a ResourceRef
    assert_eq!(
        cidr,
        Some(&Value::String("10.0.1.0/24".to_string())),
        "?? should return default when left is an unresolved ResourceRef"
    );
}

#[test]
fn exports_cross_file_binding_detection() {
    // Simulate cross-file: exports.crn parsed WITHOUT the let binding
    let exports_input = r#"
exports {
  vpc_id = vpc.vpc_id
}
"#;
    let exports_parsed = parse(exports_input, &ProviderContext::default()).unwrap();
    eprintln!("export_params: {:?}", exports_parsed.export_params);
    assert_eq!(exports_parsed.export_params.len(), 1);
    // Check if the value is a ResourceRef
    let value = exports_parsed.export_params[0].value.as_ref().unwrap();
    eprintln!("value: {:?}", value);
    let is_ref = matches!(value, Value::ResourceRef { .. });
    eprintln!("is_ref: {}", is_ref);

    // Now simulate merged ParsedFile with binding from main.crn
    let main_input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}
"#;
    let main_parsed = parse(main_input, &ProviderContext::default()).unwrap();

    // Merge like config_loader does
    let mut merged = main_parsed;
    merged.export_params.extend(exports_parsed.export_params);

    let unused = crate::validation::check_unused_bindings(&merged);
    assert!(
        unused.is_empty(),
        "vpc should not be unused when referenced from exports in a separate file, got: {:?}",
        unused
    );
}

#[test]
fn coalesce_operator_returns_left_when_resolved() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
  cidr_block = '10.1.0.0/16' ?? '10.0.0.0/16'
}
"#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let cidr = parsed.resources[0].get_attr("cidr_block");
    assert_eq!(
        cidr,
        Some(&Value::String("10.1.0.0/16".to_string())),
        "?? should return left when it's resolved"
    );
}

#[test]
fn upstream_state_refs_emit_no_parser_warnings() {
    // Field validity against upstream `exports { }` is now checked
    // statically by the `upstream_exports` module. The parser itself
    // stays silent about upstream_state references — the old "validate
    // does not inspect" soft warning is gone.
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }
        let network = upstream_state {
            source = "../network"
        }

        for name, _ in orgs.accounts {
            awscc.ec2.Vpc {
                name = name
                cidr_block = '10.0.0.0/16'
            }
        }

        awscc.ec2.SecurityGroup {
            group_description = "Web SG"
            vpc_id = network.vpc_id
        }
    "#;

    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let upstream_warnings: Vec<&ParseWarning> = parsed
        .warnings
        .iter()
        .filter(|w| w.message.contains("upstream_state"))
        .collect();
    assert!(
        upstream_warnings.is_empty(),
        "parser should emit no upstream_state warnings, got: {:?}",
        upstream_warnings
    );
    assert!(
        parsed
            .warnings
            .iter()
            .all(|w| !w.message.contains("known after apply")),
        "deferred for-iterable must no longer emit 'known after apply', got: {:?}",
        parsed.warnings
    );
}

#[test]
fn expand_deferred_for_with_remote_bindings() {
    // Parse a for-expression that references an upstream_state list.
    // Initially deferred (no remote values available at parse time).
    // Then expand with remote_bindings and verify concrete resources are created.
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }

        for account_id in orgs.accounts {
            awscc.sso.Assignment {
                instance_arn = 'arn:aws:sso:::instance/ssoins-12345'
                target_id = account_id
                target_type = 'AWS_ACCOUNT'
            }
        }
    "#;

    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.deferred_for_expressions.len(), 1);
    assert_eq!(parsed.resources.len(), 0, "no resources before expansion"); // allow: direct — fixture test inspection

    // Simulate loading upstream_state with actual values
    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    let mut orgs_attrs = HashMap::new();
    orgs_attrs.insert(
        "accounts".to_string(),
        Value::List(vec![
            Value::String("111111111111".to_string()),
            Value::String("222222222222".to_string()),
        ]),
    );
    remote_bindings.insert("orgs".to_string(), orgs_attrs);

    // Expand deferred for-expressions
    parsed.expand_deferred_for_expressions(&remote_bindings);

    // Deferred should be resolved
    assert_eq!(
        parsed.deferred_for_expressions.len(),
        0,
        "deferred should be empty after expansion"
    );
    // Warning should be removed
    assert!(
        parsed.warnings.is_empty(),
        "warning should be removed after expansion, got: {:?}",
        parsed.warnings
    );
    // Two concrete resources should be generated
    assert_eq!(
        parsed.resources.len(), // allow: direct — fixture test inspection
        2,
        "should have 2 expanded resources"
    );

    // Verify the expanded resources have substituted values
    let r0 = &parsed.resources[0];
    assert_eq!(r0.id.resource_type, "sso.Assignment");
    let target_id_0 = r0.get_attr("target_id");
    assert_eq!(
        target_id_0,
        Some(&Value::String("111111111111".to_string())),
        "target_id should be substituted with actual account ID"
    );

    let r1 = &parsed.resources[1];
    let target_id_1 = r1.get_attr("target_id");
    assert_eq!(
        target_id_1,
        Some(&Value::String("222222222222".to_string())),
    );
}

#[test]
fn expand_deferred_for_no_remote_data_stays_deferred() {
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }

        for account_id in orgs.accounts {
            awscc.sso.Assignment {
                target_id = account_id
            }
        }
    "#;

    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.deferred_for_expressions.len(), 1);

    // Empty remote_bindings — upstream hasn't been applied yet
    let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    parsed.expand_deferred_for_expressions(&remote_bindings);

    // Should remain deferred
    assert_eq!(
        parsed.deferred_for_expressions.len(),
        1,
        "should stay deferred when remote data not available"
    );
    assert_eq!(parsed.resources.len(), 0); // allow: direct — fixture test inspection
}

#[test]
fn expand_deferred_for_map_binding_substitutes_key_and_value() {
    // Map binding `for k, v in orgs.accounts` should expand each entry with
    // both the key and value variables available.
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }

        for name, account_id in orgs.accounts {
            awscc.sso.Assignment {
                target_id = account_id
                target_name = name
            }
        }
    "#;

    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.deferred_for_expressions.len(), 1);

    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    let mut accounts: IndexMap<String, Value> = IndexMap::new();
    accounts.insert(
        "prod".to_string(),
        Value::String("111111111111".to_string()),
    );
    accounts.insert("dev".to_string(), Value::String("222222222222".to_string()));
    let mut orgs_attrs = HashMap::new();
    orgs_attrs.insert("accounts".to_string(), Value::Map(accounts));
    remote_bindings.insert("orgs".to_string(), orgs_attrs);

    parsed.expand_deferred_for_expressions(&remote_bindings);

    assert_eq!(parsed.deferred_for_expressions.len(), 0);
    assert_eq!(parsed.resources.len(), 2); // allow: direct — fixture test inspection

    // Verify both key and value are substituted.
    let mut by_name: HashMap<String, &Resource> = HashMap::new();
    for r in &parsed.resources {
        if let Some(Value::String(s)) = r.get_attr("target_name") {
            by_name.insert(s.clone(), r);
        }
    }
    let prod = by_name.get("prod").expect("prod entry");
    assert_eq!(
        prod.get_attr("target_id"),
        Some(&Value::String("111111111111".to_string()))
    );
    let dev = by_name.get("dev").expect("dev entry");
    assert_eq!(
        dev.get_attr("target_id"),
        Some(&Value::String("222222222222".to_string()))
    );
}

#[test]
fn expand_deferred_for_indexed_binding_substitutes_index_and_value() {
    // Indexed binding `for (i, x) in list` must substitute BOTH the index
    // and value variables. Prior to the fix both vars shared the same
    // placeholder, causing the index to receive the item value.
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }

        for (i, account_id) in orgs.accounts {
            awscc.sso.Assignment {
                target_id = account_id
                position = i
            }
        }
    "#;

    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.deferred_for_expressions.len(), 1);

    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    let mut orgs_attrs = HashMap::new();
    orgs_attrs.insert(
        "accounts".to_string(),
        Value::List(vec![
            Value::String("111111111111".to_string()),
            Value::String("222222222222".to_string()),
        ]),
    );
    remote_bindings.insert("orgs".to_string(), orgs_attrs);

    parsed.expand_deferred_for_expressions(&remote_bindings);

    assert_eq!(parsed.resources.len(), 2); // allow: direct — fixture test inspection
    assert_eq!(
        parsed.resources[0].get_attr("target_id"),
        Some(&Value::String("111111111111".to_string()))
    );
    assert_eq!(
        parsed.resources[0].get_attr("position"),
        Some(&Value::Int(0)),
        "index should be 0, not the item value"
    );
    assert_eq!(
        parsed.resources[1].get_attr("target_id"),
        Some(&Value::String("222222222222".to_string()))
    );
    assert_eq!(
        parsed.resources[1].get_attr("position"),
        Some(&Value::Int(1))
    );
}

#[test]
fn expand_deferred_for_substitutes_placeholder_inside_interpolation() {
    // The loop var may appear inside a string interpolation like "acct-${id}".
    // Placeholder substitution must recurse into Value::Interpolation parts,
    // otherwise the rendered resource ships the raw placeholder string.
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }

        for account_id in orgs.accounts {
            awscc.sso.Assignment {
                target_id = account_id
                label = "acct-${account_id}"
            }
        }
    "#;

    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.deferred_for_expressions.len(), 1);

    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    let mut orgs_attrs = HashMap::new();
    orgs_attrs.insert(
        "accounts".to_string(),
        Value::List(vec![Value::String("111111111111".to_string())]),
    );
    remote_bindings.insert("orgs".to_string(), orgs_attrs);

    parsed.expand_deferred_for_expressions(&remote_bindings);
    assert_eq!(parsed.resources.len(), 1); // allow: direct — fixture test inspection

    // label must have the placeholder substituted in the interpolation.
    let label = parsed.resources[0].get_attr("label");
    let rendered = match label {
        Some(Value::Interpolation(parts)) => {
            let mut s = String::new();
            for p in parts {
                match p {
                    crate::resource::InterpolationPart::Literal(lit) => s.push_str(lit),
                    crate::resource::InterpolationPart::Expr(Value::String(v)) => s.push_str(v),
                    _ => s.push_str("<expr>"),
                }
            }
            s
        }
        Some(Value::String(s)) => s.clone(),
        other => panic!("unexpected label shape: {:?}", other),
    };
    assert!(
        rendered.contains("111111111111"),
        "interpolation should contain substituted account id, got: {}",
        rendered
    );
    assert!(
        !rendered.contains(DEFERRED_UPSTREAM_PLACEHOLDER),
        "placeholder must not leak into rendered label, got: {}",
        rendered
    );
}

#[test]
fn expand_deferred_for_simple_binding_with_map_iterable_warns() {
    // Simple binding but upstream resolves to a map — mismatch should warn
    // and leave deferred.
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }

        for account_id in orgs.accounts {
            awscc.sso.Assignment {
                target_id = account_id
            }
        }
    "#;

    let mut parsed = parse(input, &ProviderContext::default()).unwrap();

    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    let mut accounts: IndexMap<String, Value> = IndexMap::new();
    accounts.insert(
        "prod".to_string(),
        Value::String("111111111111".to_string()),
    );
    let mut orgs_attrs = HashMap::new();
    orgs_attrs.insert("accounts".to_string(), Value::Map(accounts));
    remote_bindings.insert("orgs".to_string(), orgs_attrs);

    parsed.expand_deferred_for_expressions(&remote_bindings);

    assert_eq!(
        parsed.resources.len(), // allow: direct — fixture test inspection
        0,
        "simple binding with map iterable should not silently expand"
    );
    assert_eq!(parsed.deferred_for_expressions.len(), 1);
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.message.contains("expected list")),
        "should warn about list vs map shape mismatch, got: {:?}",
        parsed.warnings
    );
}

#[test]
fn expand_deferred_for_map_binding_with_list_iterable_warns() {
    // Map binding but upstream resolves to a list — mismatch should produce
    // a warning and leave the for-expression deferred (do not silently expand).
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }

        for name, account_id in orgs.accounts {
            awscc.sso.Assignment {
                target_id = account_id
            }
        }
    "#;

    let mut parsed = parse(input, &ProviderContext::default()).unwrap();

    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    let mut orgs_attrs = HashMap::new();
    orgs_attrs.insert(
        "accounts".to_string(),
        Value::List(vec![
            Value::String("111111111111".to_string()),
            Value::String("222222222222".to_string()),
        ]),
    );
    remote_bindings.insert("orgs".to_string(), orgs_attrs);

    parsed.expand_deferred_for_expressions(&remote_bindings);

    // Mismatch: should NOT expand silently with numeric indices
    assert_eq!(
        parsed.resources.len(), // allow: direct — fixture test inspection
        0,
        "map binding with list iterable should not silently expand"
    );
    assert_eq!(
        parsed.deferred_for_expressions.len(),
        1,
        "should remain deferred on shape mismatch"
    );
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.message.contains("expected map") || w.message.contains("shape")),
        "should warn about shape mismatch, got: {:?}",
        parsed.warnings
    );
    // The parse-time "not yet available" warning should be replaced by the
    // more specific shape-mismatch warning (not kept alongside).
    assert!(
        !parsed
            .warnings
            .iter()
            .any(|w| w.message.contains("not yet available")
                || w.message.contains("validate does not inspect")),
        "parse-time warning should be replaced, got: {:?}",
        parsed.warnings
    );
}

#[test]
fn parses_upstream_state_expr_with_source() {
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).expect("parse should succeed");
    assert_eq!(parsed.upstream_states.len(), 1);
    let us = &parsed.upstream_states[0];
    assert_eq!(us.binding, "orgs");
    assert_eq!(us.source, std::path::PathBuf::from("../organizations"));
}

#[test]
fn old_top_level_upstream_state_syntax_is_rejected() {
    // The pre-#1926 form `upstream_state "name" { ... }` was a top-level
    // statement; with the let-binding form it should no longer parse.
    let input = r#"
        upstream_state "orgs" {
            source = "../organizations"
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "old top-level upstream_state syntax must be rejected, got: {:?}",
        result.ok().map(|p| p.upstream_states)
    );
}

#[test]
fn remote_state_keyword_is_no_longer_recognized() {
    let input = r#"
        let orgs = remote_state { path = "./foo.json" }
    "#;
    let err = parse(input, &ProviderContext::default())
        .expect_err("remote_state must be a parse error now");
    let msg = err.to_string();
    assert!(
        msg.contains("remote_state") && msg.contains("upstream_state"),
        "error should guide users to upstream_state, got: {msg}",
    );
}

#[test]
fn upstream_state_missing_source_is_error() {
    let input = r#"let orgs = upstream_state { }"#;
    let err = parse(input, &ProviderContext::default())
        .expect_err("missing source must be a parse error");
    let msg = err.to_string();
    assert!(
        msg.contains("upstream_state") && msg.contains("source") && msg.contains("orgs"),
        "error should mention upstream_state, binding, and source: {msg}",
    );
}

#[test]
fn upstream_state_source_must_be_string() {
    let input = r#"let orgs = upstream_state { source = 42 }"#;
    let err = parse(input, &ProviderContext::default())
        .expect_err("non-string source must be a parse error");
    let msg = err.to_string();
    assert!(
        msg.contains("source") && msg.contains("orgs"),
        "error should mention source and binding: {msg}",
    );
}

#[test]
fn upstream_state_unknown_attribute_is_error() {
    let input = r#"
        let orgs = upstream_state {
            source = "../foo"
            backend = "s3"
        }
    "#;
    let err = parse(input, &ProviderContext::default())
        .expect_err("unknown attribute must be a parse error");
    let msg = err.to_string();
    assert!(
        msg.contains("backend") && msg.contains("orgs"),
        "error should mention the unknown attribute and binding: {msg}",
    );
}

#[test]
fn upstream_state_duplicate_binding_is_error() {
    let input = r#"
        let orgs = upstream_state { source = "../a" }
        let orgs = upstream_state { source = "../b" }
    "#;
    let err = parse(input, &ProviderContext::default())
        .expect_err("duplicate upstream_state binding must be a parse error");
    match &err {
        ParseError::DuplicateBinding { name, .. } => {
            assert_eq!(name, "orgs");
        }
        other => panic!("Expected DuplicateBinding error, got: {other}"),
    }
}

// A dotted reference `orgs.accounts` is only valid when `orgs` is declared
// somewhere in scope (`let`, `upstream_state`, `read`, module import,
// function, or for/if structural binding). Referring to a name that isn't
// bound anywhere must be a hard error, not a deferred warning.

#[test]
fn undefined_identifier_in_for_iterable_is_error() {
    let input = r#"
        for name, account_id in orgs.accounts {
            aws.s3_bucket {
                name = name
            }
        }
    "#;
    // Iterable-binding validation runs in `check_identifier_scope`
    // on the merged directory-level `ParsedFile`, so that cross-file
    // `upstream_state` bindings in sibling files aren't rejected during
    // per-file parsing.
    let parsed = parse(input, &ProviderContext::default())
        .expect("single-file parse must not reject cross-file iterables");
    let errs = check_identifier_scope(&parsed);
    assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
    match &errs[0] {
        ParseError::UndefinedIdentifier { name, .. } => {
            assert_eq!(name, "orgs");
        }
        other => panic!("Expected UndefinedIdentifier, got: {other}"),
    }
}

#[test]
fn undefined_identifier_error_suggests_close_match() {
    // Regression for #2038. When a typo has a close edit-distance match
    // among the in-scope bindings, the error should name it so the user
    // doesn't have to guess which binding they meant.
    let input = r#"
        let orgs = upstream_state { source = "../a" }
        for _, id in org.accounts {
            aws.s3_bucket {
                name = id
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default())
        .expect("single-file parse must not reject cross-file iterables");
    let errs = check_identifier_scope(&parsed);
    assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
    let msg = errs[0].to_string();
    assert!(
        msg.contains("`org`"),
        "error should quote the unknown name, got: {msg}"
    );
    assert!(
        msg.contains("Did you mean `orgs`") || msg.contains("Did you mean 'orgs'"),
        "error should suggest the close match 'orgs', got: {msg}"
    );
}

#[test]
fn undefined_identifier_error_lists_in_scope_names_without_close_match() {
    // When nothing is close, fall back to listing the concrete in-scope
    // names so the reader learns what _is_ available. The abstract
    // "no let/upstream_state/..." kind enumeration alone is noise.
    let input = r#"
        let orgs = upstream_state { source = "../a" }
        let admins = upstream_state { source = "../b" }
        for _, id in xyzzy.accounts {
            aws.s3_bucket {
                name = id
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default())
        .expect("single-file parse must not reject cross-file iterables");
    let errs = check_identifier_scope(&parsed);
    assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
    let msg = errs[0].to_string();
    assert!(
        msg.contains("`xyzzy`"),
        "error should quote the unknown name, got: {msg}"
    );
    assert!(
        msg.contains("orgs") && msg.contains("admins"),
        "error should list in-scope names (orgs, admins), got: {msg}"
    );
    assert!(
        !msg.contains("Did you mean"),
        "no close match exists; there should be no 'Did you mean' line, got: {msg}"
    );
}

#[test]
fn bare_identifier_iterable_is_reported_as_undefined_not_string() {
    // Regression for #2101. When the iterable is a bare undeclared
    // identifier — `for ... in org { ... }` rather than the dotted
    // `org.accounts` — the parser previously reported
    // `iterable is string "org" (expected map)`, calling the identifier
    // a string and leaving the user with no did-you-mean.
    //
    // The fix records these as `DeferredForExpression` so
    // `check_identifier_scope` validates them against the merged
    // directory-wide binding set (mirrors the dotted-form path). That
    // gives us cross-file visibility for the did-you-mean candidates.
    let input = r#"
        let orgs = upstream_state { source = "../a" }
        for _, id in org {
            aws.s3_bucket {
                name = id
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default())
        .expect("single-file parse must not reject bare-iterable identifiers; the cross-file check runs later");
    let errs = check_identifier_scope(&parsed);
    assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
    let err = &errs[0];
    let msg = err.to_string();
    assert!(
        matches!(err, ParseError::UndefinedIdentifier { .. }),
        "expected UndefinedIdentifier, got: {err:?}"
    );
    assert!(
        msg.contains("`org`"),
        "error should quote the identifier, got: {msg}"
    );
    assert!(
        !msg.contains("\"org\""),
        "error must not render the identifier as a quoted string literal, got: {msg}"
    );
    assert!(
        msg.contains("Did you mean `orgs`") || msg.contains("Did you mean 'orgs'"),
        "error should suggest the close match 'orgs' via #2038 plumbing, got: {msg}"
    );
}

#[test]
fn forward_reference_to_later_let_is_allowed() {
    // `foo.id` refers to `let foo = ...` declared after the first resource.
    // This is a legitimate forward reference that the second-pass resolver
    // handles.
    let input = r#"
        let bucket = aws.s3_bucket {
            name = foo.id
        }
        let foo = aws.s3_bucket {
            name = "foo-bucket"
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "Forward reference to later `let` must still parse: {:?}",
        result.err()
    );
}

#[test]
fn backward_reference_to_resource_attr_is_allowed() {
    // `bucket.id` — `bucket` is defined; `id` is populated after apply.
    // This is the legitimate "known after apply" case.
    let input = r#"
        let bucket = aws.s3_bucket {
            name = "my-bucket"
        }
        aws.s3_bucket_policy {
            name = "policy"
            bucket_name = bucket.id
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "Reference to declared binding's attribute must parse: {:?}",
        result.err()
    );
}

#[test]
fn for_discard_pattern_simple_parses() {
    // `for _ in xs` should parse — the loop variable is intentionally unused.
    let input = r#"
        for _ in [1, 2, 3] {
            awscc.ec2.Vpc {
                cidr_block = '10.0.0.0/16'
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "discard in simple for-binding must parse: {:?}",
        result.err()
    );
}

#[test]
fn for_discard_pattern_map_key_parses() {
    // `for _, v in m` — discard the map key, use only the value.
    let input = r#"
        let things = { a = 1, b = 2 }
        for _, value in things {
            awscc.ec2.Vpc {
                cidr_block = '10.0.0.0/16'
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "discard in map-form key position must parse: {:?}",
        result.err()
    );
}

#[test]
fn for_discard_pattern_map_value_parses() {
    let input = r#"
        let things = { a = 1, b = 2 }
        for key, _ in things {
            awscc.ec2.Vpc {
                cidr_block = '10.0.0.0/16'
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "discard in map-form value position must parse: {:?}",
        result.err()
    );
}

#[test]
fn for_discard_pattern_indexed_parses() {
    let input = r#"
        for (_, item) in [1, 2, 3] {
            awscc.ec2.Vpc {
                cidr_block = '10.0.0.0/16'
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "discard in indexed-form must parse: {:?}",
        result.err()
    );
}

#[test]
fn for_discard_pattern_cannot_be_referenced() {
    // Using `_` on the RHS should error — it's not a binding, it's a
    // discard marker. This mirrors `let _ = expr`.
    let input = r#"
        for _, v in { a = 1 } {
            awscc.ec2.Vpc {
                name = _
                cidr_block = '10.0.0.0/16'
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_err(),
        "referencing a discard binding should error, got: {:?}",
        result
    );
}

#[test]
fn for_unused_binding_warns_simple() {
    // Simple-form loop variable never referenced inside the body — warn.
    let input = r#"
        for item in [1, 2, 3] {
            awscc.ec2.Vpc {
                cidr_block = '10.0.0.0/16'
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let unused: Vec<_> = parsed
        .warnings
        .iter()
        .filter(|w| w.message.contains("unused") && w.message.contains("item"))
        .collect();
    assert_eq!(
        unused.len(),
        1,
        "expected one unused-for-binding warning, got: {:?}",
        parsed.warnings
    );
}

#[test]
fn for_used_binding_no_warning() {
    // Binding is referenced in body — no warning.
    let input = r#"
        for item in [1, 2, 3] {
            awscc.ec2.Vpc {
                name = item
                cidr_block = '10.0.0.0/16'
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    assert!(
        !parsed
            .warnings
            .iter()
            .any(|w| w.message.contains("unused") && w.message.contains("item")),
        "expected no unused warning when binding is used, got: {:?}",
        parsed.warnings
    );
}

#[test]
fn for_unused_map_key_warns_only_key() {
    // Only the map key is unused — warn for key, not value.
    let input = r#"
        let things = { a = 1, b = 2 }
        for name, account_id in things {
            awscc.ec2.Vpc {
                cidr_block = account_id
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let unused: Vec<_> = parsed
        .warnings
        .iter()
        .filter(|w| w.message.contains("unused"))
        .collect();
    assert_eq!(
        unused.len(),
        1,
        "expected one warning for unused key, got: {:?}",
        parsed.warnings
    );
    assert!(
        unused[0].message.contains("name"),
        "expected warning to mention 'name', got: {}",
        unused[0].message
    );
    assert!(
        !unused[0].message.contains("account_id"),
        "warning should not mention used binding, got: {}",
        unused[0].message
    );
}

#[test]
fn for_discard_binding_no_unused_warning() {
    // `_` discard should suppress the unused-warning check.
    let input = r#"
        let things = { a = 1, b = 2 }
        for _, account_id in things {
            awscc.ec2.Vpc {
                cidr_block = account_id
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    assert!(
        !parsed.warnings.iter().any(|w| w.message.contains("unused")),
        "discard binding should suppress unused warning, got: {:?}",
        parsed.warnings
    );
}

#[test]
fn reference_to_upstream_state_binding_is_allowed() {
    // `orgs` IS declared via upstream_state. The field (`accounts`) may
    // not yet be loaded — that stays as a deferred warning, not an error.
    let input = r#"
        let orgs = upstream_state {
            source = "../organizations"
        }
        for name, account_id in orgs.accounts {
            aws.s3_bucket {
                name = name
            }
        }
    "#;
    let result = parse(input, &ProviderContext::default());
    assert!(
        result.is_ok(),
        "Reference to upstream_state binding must parse: {:?}",
        result.err()
    );
}

/// Issue #2229 acceptance criterion 3: the "was this attribute written
/// as a quoted literal?" bit must survive an anonymous-resource rename.
/// Anonymous resources start with an empty name; the post-parse
/// identifier pass rewrites that name. A side-table keyed by
/// `ResourceId` would silently miss after the rename. Co-locating
/// the bit on the `Resource` (the same struct that carries the
/// attributes) makes it impossible to lose.
#[test]
fn quoted_literal_marker_survives_anonymous_resource_rename() {
    let input = r#"
        aws.sso_admin.principal_assignment {
            target_type = "AWS_ACCOUNT"
        }
    "#;
    let mut parsed = parse(input, &ProviderContext::default()).unwrap();
    assert_eq!(parsed.resources.len(), 1); // allow: direct — fixture test inspection

    // Simulate the rename that compute_anonymous_identifiers would
    // perform: the pending name becomes a hash-based bound identifier.
    let resource = &mut parsed.resources[0]; // allow: direct — fixture test inspection
    assert!(resource.id.name.is_pending());
    resource.id.name = crate::resource::ResourceName::Bound("hash123".to_string());

    // The "was quoted" marker must still be reachable on the
    // resource — it co-locates with the attributes, not in a
    // side-table keyed by the (now-stale) ResourceId.
    assert!(
        resource.quoted_string_attrs.contains("target_type"),
        "quoted-literal attribute name must survive rename; got {:?}",
        resource.quoted_string_attrs
    );
}

/// Issue #2094 / #2229: distinguish quoted string literals from
/// bare identifiers and namespaced identifiers at the parser level,
/// so downstream enum diagnostics can report shape mismatches
/// ("got a string literal") vs. variant mismatches ("invalid enum
/// variant"). After #2229 the marker lives on the `Resource` that
/// owns the attributes (`Resource.quoted_string_attrs`); the
/// previous `string_literal_paths` side-table is gone.
#[test]
fn quoted_string_attrs_distinguish_quoted_from_bare_and_namespaced() {
    let input = r#"
        let a = aws.sso_admin.principal_assignment {
            target_type = "aaa"
        }

        let b = aws.sso_admin.principal_assignment {
            target_type = AWS_ACCOUNT
        }

        let c = aws.sso_admin.principal_assignment {
            target_type = awscc.sso.Assignment.TargetType.AWS_ACCOUNT
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();

    let quoted_of = |binding: &str| -> bool {
        parsed
            .resources
            .iter()
            .find(|r| r.binding.as_deref() == Some(binding))
            .map(|r| r.quoted_string_attrs.contains("target_type"))
            .unwrap_or(false)
    };

    // Quoted literal carries the marker.
    assert!(
        quoted_of("a"),
        "quoted literal `target_type = \"aaa\"` must be marked"
    );
    // Bare identifier and namespaced identifier do not.
    assert!(!quoted_of("b"), "bare identifier must NOT be marked");
    assert!(!quoted_of("c"), "namespaced identifier must NOT be marked");
}

#[test]
fn quoted_string_attrs_are_top_level_only() {
    // The quoted-bit currently scopes to the resource's top-level
    // attributes. Nested-block attributes (`rules { protocol = "tcp" }`)
    // and list / map elements are intentionally not recorded — no
    // current consumer needs them, and tracking them would re-introduce
    // the path-keyed shape that #2229 removed. This test pins that
    // contract so a future "let's also track nested" change is a
    // visible API decision rather than a silent broadening.
    let input = r#"
        let sg = aws.ec2.SecurityGroup {
            name = "sg-1"
            rules {
                protocol = "tcp"
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let sg = parsed
        .resources
        .iter()
        .find(|r| r.binding.as_deref() == Some("sg"))
        .expect("sg resource present");
    assert!(sg.quoted_string_attrs.contains("name"));
    // `protocol` lives inside the `rules` block; only top-level
    // attribute names ("name", "rules") are tracked.
    assert!(!sg.quoted_string_attrs.contains("protocol"));
}

#[test]
fn quoted_string_attrs_skipped_for_interpolated_strings() {
    // An interpolated string is not a "plain" literal — users who write
    // "${x}" are constructing a value, not typing an enum by mistake.
    let input = r#"
        let x = "env"
        let r = aws.s3_bucket {
            name = "bucket-${x}"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let r = parsed
        .resources
        .iter()
        .find(|r| r.binding.as_deref() == Some("r"))
        .expect("r resource present");
    // Interpolations parse to `Value::Interpolation`, not a plain
    // string, so they are never recorded in `quoted_string_attrs`.
    assert!(
        !r.quoted_string_attrs.contains("name"),
        "interpolated string must not be tagged as a quoted literal; got {:?}",
        r.quoted_string_attrs
    );
}

/// The payload of `Value::Map` must preserve the source order of the
/// keys the user wrote — top-level map literals included.
#[test]
fn value_map_preserves_insertion_order() {
    let input = r#"
        let m = {
            z_first = "1"
            a_second = "2"
            m_third = "3"
            b_fourth = "4"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let Some(Value::Map(map)) = parsed.variables.get("m") else {
        panic!("expected variables['m'] to be a Value::Map");
    };
    let keys: Vec<&str> = map.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["z_first", "a_second", "m_third", "b_fourth"],
        "Value::Map must preserve source key order; got {keys:?}"
    );
}

/// `ProviderConfig.default_tags` must preserve the source order in
/// which the user wrote tag keys. The map is extracted from a
/// `default_tags = { ... }` block, so the same `Value::Map`
/// guarantee applies.
#[test]
fn provider_config_default_tags_preserve_insertion_order() {
    let input = r#"
        provider test {
            source = "x/y"
            version = "0.1"
            region = "ap-northeast-1"
            default_tags = {
                z_team = "infra"
                a_env = "prod"
                m_owner = "ops"
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let pc = parsed
        .providers
        .first()
        .expect("expected one provider config");
    let keys: Vec<&str> = pc.default_tags.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["z_team", "a_env", "m_owner"],
        "ProviderConfig.default_tags must preserve source key order; got {keys:?}"
    );
}

/// `ProviderConfig.attributes` must preserve source order so that
/// anything re-rendering provider blocks (formatter, diagnostics)
/// sees a deterministic order.
#[test]
fn provider_config_attributes_preserve_insertion_order() {
    let input = r#"
        provider test {
            source = "x/y"
            version = "0.1"
            z_extra = "1"
            a_extra = "2"
            m_extra = "3"
            region = "ap-northeast-1"
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let pc = parsed
        .providers
        .first()
        .expect("expected one provider config");
    let keys: Vec<&str> = pc.attributes.keys().map(String::as_str).collect();
    // `source` and `version` are stripped from `attributes` (extracted
    // separately into ProviderConfig fields), so the surviving keys
    // are the user-authored order minus those two.
    assert_eq!(
        keys,
        vec!["z_extra", "a_extra", "m_extra", "region"],
        "ProviderConfig.attributes must preserve source key order; got {keys:?}"
    );
}

/// `ParsedFile.variables` must preserve the order in which top-level
/// `let` bindings were declared so that iteration matches source
/// order. Later bindings can reference earlier ones.
#[test]
fn parsed_file_variables_preserve_insertion_order() {
    let input = r#"
        let z_first = "1"
        let a_second = "2"
        let m_third = "3"
        let b_fourth = "4"
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let keys: Vec<&str> = parsed.variables.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["z_first", "a_second", "m_third", "b_fourth"],
        "ParsedFile.variables must preserve source order; got {keys:?}"
    );
}

/// A nested block's attributes must surface in source order on the
/// `Value::Map` payload, end-to-end through the parser.
#[test]
fn nested_block_value_map_preserves_insertion_order() {
    let input = r#"
        provider test {
            source = "x/y"
            version = "0.1"
            region = "ap-northeast-1"
        }
        let r = test.r.res {
            name = "x"
            nested {
                z_first = "1"
                a_second = "2"
                m_third = "3"
            }
        }
    "#;
    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let resource = parsed
        .resources
        .first()
        .expect("expected one resource binding");
    let nested = resource
        .get_attr("nested")
        .expect("expected `nested` attribute");
    // Nested blocks are wrapped in a List<Map> by the parser.
    let Value::List(blocks) = nested else {
        panic!("expected nested blocks to be a List, got {nested:?}");
    };
    let block = blocks.first().expect("expected one nested block");
    let Value::Map(map) = block else {
        panic!("expected nested block to be a Value::Map, got {block:?}");
    };
    let keys: Vec<&str> = map.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["z_first", "a_second", "m_third"],
        "nested block Value::Map must preserve source key order; got {keys:?}"
    );
}
