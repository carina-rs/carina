use super::*;

#[test]
fn attribute_schema_write_only_default_false() {
    let attr = AttributeSchema::new("ipv4_netmask_length", AttributeType::Int);
    assert!(!attr.write_only);
}

#[test]
fn attribute_schema_write_only_builder() {
    let attr = AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only();
    assert!(attr.write_only);
}

#[test]
fn resource_schema_data_source_default_false() {
    let schema = ResourceSchema::new("test.resource");
    assert!(!schema.data_source);
}

#[test]
fn resource_schema_as_data_source_sets_flag() {
    let schema = ResourceSchema::new("test.resource").as_data_source();
    assert!(schema.data_source);
}

#[test]
fn validate_string_type() {
    let t = AttributeType::String;
    assert!(t.validate(&Value::String("hello".to_string())).is_ok());
    assert!(t.validate(&Value::Int(42)).is_err());
}

#[test]
fn validate_string_enum_type() {
    let t = AttributeType::StringEnum {
        name: "AddressFamily".to_string(),
        values: vec!["IPv4".to_string(), "IPv6".to_string()],
        namespace: Some("awscc.ec2.ipam_pool".to_string()),
        to_dsl: None,
    };
    assert!(
        t.validate(&Value::String(
            "awscc.ec2.ipam_pool.AddressFamily.IPv4".to_string()
        ))
        .is_ok()
    );
    assert!(t.validate(&Value::String("IPv6".to_string())).is_ok());
    assert!(t.validate(&Value::String("ipv4".to_string())).is_ok());
    assert!(t.validate(&Value::String("IPv5".to_string())).is_err());
}

#[test]
fn string_enum_type_name_uses_declared_name() {
    let t = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: Some("aws.s3.Bucket".to_string()),
        to_dsl: None,
    };
    assert_eq!(t.type_name(), "VersioningStatus");
}

#[test]
fn validate_string_enum_accepts_to_dsl_alias() {
    let t = AttributeType::StringEnum {
        name: "IpProtocol".to_string(),
        values: vec![
            "tcp".to_string(),
            "udp".to_string(),
            "icmp".to_string(),
            "icmpv6".to_string(),
            "-1".to_string(),
        ],
        namespace: Some("awscc.ec2.SecurityGroup".to_string()),
        to_dsl: Some(|s: &str| match s {
            "-1" => "all".to_string(),
            _ => s.replace('-', "_"),
        }),
    };
    // Canonical value "-1" should be accepted
    assert!(t.validate(&Value::String("-1".to_string())).is_ok());
    // DSL alias "all" should be accepted
    assert!(
        t.validate(&Value::String(
            "awscc.ec2.SecurityGroup.IpProtocol.all".to_string()
        ))
        .is_ok()
    );
    // Other canonical values should still work
    assert!(t.validate(&Value::String("tcp".to_string())).is_ok());
    // Invalid values should still be rejected
    assert!(t.validate(&Value::String("invalid".to_string())).is_err());
}

#[test]
fn validate_string_enum_all_without_to_dsl_requires_explicit_variant() {
    // When StringEnum goes through the protocol layer (external process
    // providers), to_dsl and namespace are lost. Without "all" as a direct
    // variant, it cannot be accepted (issue #1428).
    let without_all = AttributeType::StringEnum {
        name: String::new(),
        values: vec![
            "tcp".to_string(),
            "udp".to_string(),
            "icmp".to_string(),
            "icmpv6".to_string(),
            "-1".to_string(),
        ],
        namespace: None,
        to_dsl: None,
    };
    // Without "all" in values and no to_dsl alias, "all" is rejected
    assert!(
        without_all
            .validate(&Value::String("all".to_string()))
            .is_err()
    );

    // With "all" added to values, it is accepted even without to_dsl
    let with_all = AttributeType::StringEnum {
        name: String::new(),
        values: vec![
            "tcp".to_string(),
            "udp".to_string(),
            "icmp".to_string(),
            "icmpv6".to_string(),
            "-1".to_string(),
            "all".to_string(),
        ],
        namespace: None,
        to_dsl: None,
    };
    assert!(with_all.validate(&Value::String("all".to_string())).is_ok());
}

#[test]
fn validate_string_enum_accepts_values_with_dots() {
    // Values like "ipsec.1" contain dots that should not be treated as
    // namespace separators (issue #611)
    let t = AttributeType::StringEnum {
        name: "Type".to_string(),
        values: vec!["ipsec.1".to_string()],
        namespace: Some("awscc.ec2.vpn_gateway".to_string()),
        to_dsl: None,
    };
    // Quoted string with dot should match directly
    assert!(t.validate(&Value::String("ipsec.1".to_string())).is_ok());
    // Fully qualified form should also be accepted
    assert!(
        t.validate(&Value::String(
            "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
        ))
        .is_ok()
    );
    // Invalid value should still be rejected
    assert!(t.validate(&Value::String("ipsec.2".to_string())).is_err());
}

#[test]
fn invalid_enum_error_preserves_user_typed_string_literal() {
    // Regression for #2077. A quoted string literal like `target_type = "aaa"`
    // should surface in the error as the typed value, not as the synthesized
    // namespaced form `awscc.sso.Assignment.TargetType.aaa`.
    let t = AttributeType::StringEnum {
        name: "TargetType".to_string(),
        values: vec!["AWS_ACCOUNT".to_string()],
        namespace: Some("awscc.sso.Assignment".to_string()),
        to_dsl: None,
    };
    let err = t.validate(&Value::String("aaa".to_string())).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("'aaa'"),
        "error should quote the user's typed value, got: {msg}"
    );
    assert!(
        !msg.contains("awscc.sso.Assignment.TargetType.aaa"),
        "error must not leak the synthesized namespaced form, got: {msg}"
    );
}

#[test]
fn invalid_enum_error_names_the_enum_type_and_fully_qualified_variants() {
    // Regression for #2095. The message must identify which enum is
    // expected and list allowed variants in their fully-qualified form
    // so the user can copy-paste one into their .crn without having to
    // synthesize the namespace prefix.
    let t = AttributeType::StringEnum {
        name: "TargetType".to_string(),
        values: vec!["AWS_ACCOUNT".to_string()],
        namespace: Some("awscc.sso.Assignment".to_string()),
        to_dsl: None,
    };
    let err = t.validate(&Value::String("aaa".to_string())).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("TargetType"),
        "error should name the enum type, got: {msg}"
    );
    assert!(
        msg.contains("awscc.sso.Assignment.TargetType.AWS_ACCOUNT"),
        "error should list variants in fully-qualified form, got: {msg}"
    );
}

#[test]
fn with_attribute_adds_attribute_name_to_enum_error() {
    // Regression for #2098. When a caller knows which attribute produced
    // the error, wrapping it with `with_attribute` must surface the name
    // in the rendered message so the reader can locate it in their .crn.
    let t = AttributeType::StringEnum {
        name: "TargetType".to_string(),
        values: vec!["AWS_ACCOUNT".to_string()],
        namespace: Some("awscc.sso.Assignment".to_string()),
        to_dsl: None,
    };
    let err = t
        .validate(&Value::String("aaa".to_string()))
        .unwrap_err()
        .with_attribute("target_id");
    let msg = err.to_string();
    assert!(
        msg.contains("'target_id'"),
        "error should quote the attribute name, got: {msg}"
    );
    assert!(
        msg.contains("TargetType"),
        "error should still name the enum type, got: {msg}"
    );
    assert!(
        msg.contains("'aaa'"),
        "error should still quote the user value, got: {msg}"
    );
}

#[test]
fn with_attribute_is_noop_for_variants_that_dont_carry_attribute() {
    // `with_attribute` is a no-op for TypeError variants that don't
    // carry an attribute slot — it must not panic or corrupt the
    // message.
    let original = TypeError::TypeMismatch {
        expected: "String".to_string(),
        got: "Int".to_string(),
    };
    let expected_msg = original.to_string();
    let wrapped = original.with_attribute("foo").to_string();
    assert_eq!(wrapped, expected_msg);
}

#[test]
fn schema_validate_wraps_enum_error_with_attribute_name() {
    // End-to-end at the ResourceSchema boundary: the attribute loop in
    // `ResourceSchema::validate` must wrap type errors with the attribute
    // name so every downstream consumer (CLI, LSP) sees the plumbed form.
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::new(
            "target_type",
            AttributeType::StringEnum {
                name: "TargetType".to_string(),
                values: vec!["AWS_ACCOUNT".to_string()],
                namespace: Some("awscc.sso.Assignment".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert("target_type".to_string(), Value::String("aaa".to_string()));
    let errs = schema.validate(&attrs).unwrap_err();
    let joined = errs
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("'target_type'"),
        "schema.validate must wrap enum errors with attribute name, got: {joined}"
    );
}

#[test]
fn string_literal_expected_enum_formats_shape_message() {
    // The new variant's message must read as a shape complaint, not as
    // "unknown variant" — that's the whole point of the PR 2094 split.
    let err = TypeError::StringLiteralExpectedEnum {
        user_typed: "aaa".to_string(),
        attribute: Some("target_type".to_string()),
        type_name: "TargetType".to_string(),
        expected: vec![ExpectedEnumVariant {
            provider: Some("awscc".to_string()),
            segments: vec!["sso".to_string(), "Assignment".to_string()],
            type_name: "TargetType".to_string(),
            value: "AWS_ACCOUNT".to_string(),
            is_alias: false,
        }],
        extra_message: None,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("got a string literal"),
        "message should name the shape mismatch, got: {msg}"
    );
    assert!(
        msg.contains("\"aaa\""),
        "message should echo the typed value in quotes, got: {msg}"
    );
    assert!(
        msg.contains("target_type") && msg.contains("TargetType"),
        "message should name attribute and enum type, got: {msg}"
    );
    assert!(
        msg.contains("awscc.sso.Assignment.TargetType.AWS_ACCOUNT"),
        "message should list the valid variants, got: {msg}"
    );
}

#[test]
fn into_string_literal_diagnostic_reshapes_invalid_enum_variant() {
    // The helper is the one bridge between the two diagnostics — the
    // shape is specifically a conversion of `InvalidEnumVariant` when
    // we know the value came from a string literal.
    let original = TypeError::InvalidEnumVariant {
        value: "aaa".to_string(),
        attribute: Some("target_type".to_string()),
        type_name: Some("TargetType".to_string()),
        expected: vec![ExpectedEnumVariant {
            provider: Some("awscc".to_string()),
            segments: vec!["sso".to_string(), "Assignment".to_string()],
            type_name: "TargetType".to_string(),
            value: "AWS_ACCOUNT".to_string(),
            is_alias: false,
        }],
    };
    let reshaped = original.into_string_literal_diagnostic();
    match reshaped {
        TypeError::StringLiteralExpectedEnum {
            user_typed,
            attribute,
            type_name,
            expected,
            extra_message,
        } => {
            assert_eq!(user_typed, "aaa");
            assert_eq!(attribute.as_deref(), Some("target_type"));
            assert_eq!(type_name, "TargetType");
            assert_eq!(expected.len(), 1);
            assert_eq!(
                expected[0].to_string(),
                "awscc.sso.Assignment.TargetType.AWS_ACCOUNT"
            );
            assert_eq!(extra_message, None);
        }
        other => panic!("expected StringLiteralExpectedEnum, got {other:?}"),
    }
}

#[test]
fn into_string_literal_diagnostic_without_type_name_passes_through() {
    // If the enum type wasn't captured (e.g. an error synthesized by a
    // plugin), there's nothing to reshape into — the original error
    // stays intact so we don't drop information on the floor.
    let original = TypeError::InvalidEnumVariant {
        value: "aaa".to_string(),
        attribute: Some("target_type".to_string()),
        type_name: None,
        expected: vec![],
    };
    let reshaped = original.into_string_literal_diagnostic();
    assert!(matches!(reshaped, TypeError::InvalidEnumVariant { .. }));
}

#[test]
fn schema_validate_with_origins_emits_string_literal_diagnostic_for_quoted_enum() {
    // `validate_with_origins` is the entry point the CLI/LSP wiring
    // calls once it knows which attributes were written as quoted
    // string literals. A quoted-literal assignment to a StringEnum
    // must reshape into `StringLiteralExpectedEnum`.
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::new(
            "target_type",
            AttributeType::StringEnum {
                name: "TargetType".to_string(),
                values: vec!["AWS_ACCOUNT".to_string()],
                namespace: Some("awscc.sso.Assignment".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert("target_type".to_string(), Value::String("aaa".to_string()));

    // String-literal origin → reshaped diagnostic
    let errs = schema
        .validate_with_origins(&attrs, &|name| name == "target_type")
        .unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, TypeError::StringLiteralExpectedEnum { .. })),
        "expected StringLiteralExpectedEnum variant, got: {errs:?}"
    );

    // No string-literal origin → classic InvalidEnumVariant (unchanged
    // behaviour for bare-identifier / namespaced inputs)
    let errs = schema
        .validate_with_origins(&attrs, &|_| false)
        .unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, TypeError::InvalidEnumVariant { .. })),
        "expected InvalidEnumVariant variant when origin is not a literal, got: {errs:?}"
    );
}

#[test]
fn schema_validate_with_origins_leaves_valid_values_alone() {
    // A valid enum value (bare or namespaced) must still pass even
    // when the caller tags it as string-literal origin — the shape
    // check is only triggered on a failing match.
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::new(
            "target_type",
            AttributeType::StringEnum {
                name: "TargetType".to_string(),
                values: vec!["AWS_ACCOUNT".to_string()],
                namespace: Some("awscc.sso.Assignment".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::String("AWS_ACCOUNT".to_string()),
    );
    assert!(
        schema.validate_with_origins(&attrs, &|_| true).is_ok(),
        "valid enum value should pass regardless of origin tag"
    );
}

#[test]
fn schema_validate_with_origins_reshapes_custom_namespaced_type() {
    // #2094 / PR 3: a quoted literal written against an
    // `AttributeType::Custom` with a namespace must also surface as
    // `StringLiteralExpectedEnum`. Custom validation returns
    // `ValidationFailed { message }` with no structured type slots, so
    // the reshape carries the original message forward in `expected`
    // while marking the variant and type name correctly.
    fn validate_mode(v: &Value) -> Result<(), String> {
        match v {
            Value::String(s) if s == "test.r.Mode.fast" => Ok(()),
            Value::String(s) => Err(format!("invalid Mode '{}': expected fast", s)),
            _ => Err("expected String".to_string()),
        }
    }
    let schema = ResourceSchema::new("test.r.mode_holder").attribute(
        AttributeSchema::new(
            "mode",
            AttributeType::Custom {
                semantic_name: Some("Mode".to_string()),
                base: Box::new(AttributeType::String),
                pattern: None,
                length: None,
                validate: validate_mode,
                namespace: Some("test.r".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    // Two-part form like "test.r.Mode.aaa" would skip the Custom
    // branch by passing validation; use a bare-shape literal that
    // resolve_enum_input can't save.
    attrs.insert("mode".to_string(), Value::String("aaa".to_string()));
    let errs = schema
        .validate_with_origins(&attrs, &|n| n == "mode")
        .unwrap_err();
    let variant_match = errs.iter().any(|e| {
        matches!(
            e,
            TypeError::StringLiteralExpectedEnum { type_name, .. }
                if type_name == "Mode"
        )
    });
    assert!(
        variant_match,
        "expected StringLiteralExpectedEnum for Custom namespaced type, got: {errs:?}"
    );
}

#[test]
fn invalid_enum_error_without_namespace_uses_bare_variants() {
    // Non-namespaced enums must keep emitting bare variant names — there's
    // no namespace to prefix with.
    let t = AttributeType::StringEnum {
        name: "Mode".to_string(),
        values: vec!["fast".to_string(), "slow".to_string()],
        namespace: None,
        to_dsl: None,
    };
    let err = t.validate(&Value::String("zzz".to_string())).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("fast") && msg.contains("slow"),
        "error should list bare variants, got: {msg}"
    );
    // Guard against accidentally printing "None.Mode.fast" or similar.
    assert!(
        !msg.contains(".Mode."),
        "non-namespaced enum must not synthesize a prefix, got: {msg}"
    );
}

#[test]
fn invalid_enum_error_preserves_bare_identifier_form() {
    // When the user types the namespaced form directly (bare identifier
    // path produces the same `Value::String(...)`), the error echoes the
    // full form back — still the "user-typed" form because that's what
    // was in the Value. This verifies the fix doesn't regress that case.
    let t = AttributeType::StringEnum {
        name: "TargetType".to_string(),
        values: vec!["AWS_ACCOUNT".to_string()],
        namespace: Some("awscc.sso.Assignment".to_string()),
        to_dsl: None,
    };
    let input = "awscc.sso.Assignment.TargetType.NOT_REAL".to_string();
    let err = t.validate(&Value::String(input.clone())).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(&input),
        "error should echo the user's namespaced input, got: {msg}"
    );
}

#[test]
fn validate_string_enum_rejects_double_namespace() {
    let t = AttributeType::StringEnum {
        name: "InstanceTenancy".to_string(),
        values: vec![
            "default".to_string(),
            "dedicated".to_string(),
            "host".to_string(),
        ],
        namespace: Some("awscc.ec2.Vpc".to_string()),
        to_dsl: None,
    };
    // Double-namespace must be rejected
    assert!(
        t.validate(&Value::String(
            "awscc.ec2.Vpc.InstanceTenancy.awscc.ec2.Vpc.InstanceTenancy.default".to_string()
        ))
        .is_err()
    );
}

#[test]
fn validate_float_type() {
    let t = AttributeType::Float;
    assert!(t.validate(&Value::Float(2.5)).is_ok());
    assert!(t.validate(&Value::Float(-0.5)).is_ok());
    assert!(t.validate(&Value::Int(42)).is_ok()); // integers are valid numbers
    assert!(t.validate(&Value::String("3.14".to_string())).is_err());
    assert!(t.validate(&Value::Bool(true)).is_err());
}

#[test]
fn validate_float_rejects_non_finite() {
    let t = AttributeType::Float;
    assert!(t.validate(&Value::Float(f64::NAN)).is_err());
    assert!(t.validate(&Value::Float(f64::INFINITY)).is_err());
    assert!(t.validate(&Value::Float(f64::NEG_INFINITY)).is_err());
}

#[test]
fn validate_int_rejects_float() {
    let t = AttributeType::Int;
    assert!(t.validate(&Value::Int(42)).is_ok());
    assert!(t.validate(&Value::Float(2.5)).is_err()); // strict integer typing
}

#[test]
fn validate_positive_int() {
    let t = types::positive_int();
    assert!(t.validate(&Value::Int(1)).is_ok());
    assert!(t.validate(&Value::Int(100)).is_ok());
    assert!(t.validate(&Value::Int(0)).is_err());
    assert!(t.validate(&Value::Int(-1)).is_err());
}

#[test]
fn validate_resource_schema() {
    let schema = ResourceSchema::new("resource")
        .attribute(AttributeSchema::new("name", AttributeType::String).required())
        .attribute(AttributeSchema::new("count", types::positive_int()))
        .attribute(AttributeSchema::new("enabled", AttributeType::Bool));

    let mut attrs = HashMap::new();
    attrs.insert("name".to_string(), Value::String("my-resource".to_string()));
    attrs.insert("count".to_string(), Value::Int(5));
    attrs.insert("enabled".to_string(), Value::Bool(true));

    assert!(schema.validate(&attrs).is_ok());
}

#[test]
fn missing_required_attribute() {
    let schema = ResourceSchema::new("bucket")
        .attribute(AttributeSchema::new("name", AttributeType::String).required());

    let attrs = HashMap::new();
    let result = schema.validate(&attrs);
    assert!(result.is_err());
}

#[test]
fn validate_cidr_type() {
    let t = types::ipv4_cidr();

    // Valid CIDRs
    assert!(
        t.validate(&Value::String("10.0.0.0/16".to_string()))
            .is_ok()
    );
    assert!(
        t.validate(&Value::String("192.168.1.0/24".to_string()))
            .is_ok()
    );
    assert!(t.validate(&Value::String("0.0.0.0/0".to_string())).is_ok());
    assert!(
        t.validate(&Value::String("255.255.255.255/32".to_string()))
            .is_ok()
    );

    // Invalid CIDRs
    assert!(t.validate(&Value::String("10.0.0.0".to_string())).is_err()); // no prefix
    assert!(
        t.validate(&Value::String("10.0.0.0/33".to_string()))
            .is_err()
    ); // prefix too large
    assert!(
        t.validate(&Value::String("10.0.0.256/16".to_string()))
            .is_err()
    ); // octet > 255
    assert!(t.validate(&Value::String("10.0.0/16".to_string())).is_err()); // only 3 octets
    assert!(t.validate(&Value::String("invalid".to_string())).is_err()); // not a CIDR
    assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
}

#[test]
fn validate_struct_type() {
    let t = AttributeType::Struct {
        name: "Ingress".to_string(),
        fields: vec![
            StructField::new("ip_protocol", AttributeType::String).required(),
            StructField::new("from_port", AttributeType::Int),
            StructField::new("to_port", AttributeType::Int),
        ],
    };

    // Valid: all required fields present
    let mut map = IndexMap::new();
    map.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    map.insert("from_port".to_string(), Value::Int(80));
    assert!(t.validate(&Value::Map(map)).is_ok());

    // Invalid: missing required field
    let empty_map = IndexMap::new();
    assert!(t.validate(&Value::Map(empty_map)).is_err());

    // Invalid: wrong type for field
    let mut bad_map = IndexMap::new();
    bad_map.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    bad_map.insert(
        "from_port".to_string(),
        Value::String("not_a_number".to_string()),
    );
    assert!(t.validate(&Value::Map(bad_map)).is_err());

    // Invalid: not a Map
    assert!(
        t.validate(&Value::String("not a struct".to_string()))
            .is_err()
    );
}

#[test]
fn struct_rejects_unknown_field() {
    let t = AttributeType::Struct {
        name: "Ingress".to_string(),
        fields: vec![
            StructField::new("ip_protocol", AttributeType::String).required(),
            StructField::new("from_port", AttributeType::Int),
            StructField::new("to_port", AttributeType::Int),
            StructField::new("cidr_ip", AttributeType::String),
        ],
    };

    // Unknown field should be rejected
    let mut map = IndexMap::new();
    map.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    map.insert(
        "unknown_field".to_string(),
        Value::String("value".to_string()),
    );
    let result = t.validate(&Value::Map(map));
    assert!(result.is_err());
    let err = result.unwrap_err();
    match &err {
        TypeError::UnknownStructField {
            struct_name,
            field,
            suggestion,
        } => {
            assert_eq!(struct_name, "Ingress");
            assert_eq!(field, "unknown_field");
            assert!(suggestion.is_none());
        }
        other => panic!("Expected UnknownStructField, got: {:?}", other),
    }
}

#[test]
fn struct_suggests_similar_field() {
    let t = AttributeType::Struct {
        name: "Ingress".to_string(),
        fields: vec![
            StructField::new("ip_protocol", AttributeType::String),
            StructField::new("from_port", AttributeType::Int),
            StructField::new("to_port", AttributeType::Int),
            StructField::new("cidr_ip", AttributeType::String),
        ],
    };

    // Typo: "ip_protcol" -> should suggest "ip_protocol"
    let mut map = IndexMap::new();
    map.insert("ip_protcol".to_string(), Value::String("tcp".to_string()));
    let result = t.validate(&Value::Map(map));
    assert!(result.is_err());
    let err = result.unwrap_err();
    match &err {
        TypeError::UnknownStructField {
            struct_name,
            field,
            suggestion,
        } => {
            assert_eq!(struct_name, "Ingress");
            assert_eq!(field, "ip_protcol");
            assert_eq!(suggestion.as_deref(), Some("ip_protocol"));
        }
        other => panic!("Expected UnknownStructField, got: {:?}", other),
    }

    // Typo: "cidr_iip" -> should suggest "cidr_ip"
    let mut map2 = IndexMap::new();
    map2.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    map2.insert(
        "cidr_iip".to_string(),
        Value::String("10.0.0.0/8".to_string()),
    );
    let result2 = t.validate(&Value::Map(map2));
    assert!(result2.is_err());
    let err2 = result2.unwrap_err();
    match &err2 {
        TypeError::UnknownStructField {
            suggestion, field, ..
        } => {
            assert_eq!(field, "cidr_iip");
            assert_eq!(suggestion.as_deref(), Some("cidr_ip"));
        }
        other => panic!("Expected UnknownStructField, got: {:?}", other),
    }
}

#[test]
fn struct_error_message_format() {
    let t = AttributeType::Struct {
        name: "SecurityGroupIngress".to_string(),
        fields: vec![
            StructField::new("vpc_id", AttributeType::String),
            StructField::new("cidr_ip", AttributeType::String),
        ],
    };

    // With suggestion
    let mut map = IndexMap::new();
    map.insert("vpc_idd".to_string(), Value::String("vpc-123".to_string()));
    let err = t.validate(&Value::Map(map)).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Unknown field 'vpc_idd' in SecurityGroupIngress, did you mean 'vpc_id'?"
    );

    // Without suggestion (completely different name)
    let mut map2 = IndexMap::new();
    map2.insert(
        "completely_different".to_string(),
        Value::String("x".to_string()),
    );
    let err2 = t.validate(&Value::Map(map2)).unwrap_err();
    assert_eq!(
        err2.to_string(),
        "Unknown field 'completely_different' in SecurityGroupIngress"
    );
}

#[test]
fn test_levenshtein_distance() {
    assert_eq!(levenshtein_distance("", ""), 0);
    assert_eq!(levenshtein_distance("abc", "abc"), 0);
    assert_eq!(levenshtein_distance("abc", ""), 3);
    assert_eq!(levenshtein_distance("", "abc"), 3);
    assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    assert_eq!(levenshtein_distance("vpc_id", "vpc_idd"), 1);
    assert_eq!(levenshtein_distance("ip_protocol", "ip_protcol"), 1);
}

#[test]
fn test_suggest_similar_name() {
    let fields = vec!["ip_protocol", "from_port", "to_port", "cidr_ip"];

    // Close match
    assert_eq!(
        suggest_similar_name("ip_protcol", &fields),
        Some("ip_protocol".to_string())
    );
    assert_eq!(
        suggest_similar_name("cidr_iip", &fields),
        Some("cidr_ip".to_string())
    );
    assert_eq!(
        suggest_similar_name("from_prot", &fields),
        Some("from_port".to_string())
    );

    // No match (too far)
    assert_eq!(suggest_similar_name("completely_unrelated", &fields), None);
}

#[test]
fn validate_list_of_struct() {
    let struct_type = AttributeType::Struct {
        name: "Ingress".to_string(),
        fields: vec![StructField::new("ip_protocol", AttributeType::String).required()],
    };
    let list_type = AttributeType::list(struct_type);

    let mut item = IndexMap::new();
    item.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
    let list = Value::List(vec![Value::Map(item)]);
    assert!(list_type.validate(&list).is_ok());

    // Invalid item in list
    let bad_list = Value::List(vec![Value::Map(IndexMap::new())]);
    assert!(list_type.validate(&bad_list).is_err());
}

#[test]
fn struct_rejects_block_syntax_single_element() {
    // Block syntax produces Value::List([Value::Map(...)]) which should be rejected
    // for bare Struct attributes
    let struct_type = AttributeType::Struct {
        name: "VersioningConfiguration".to_string(),
        fields: vec![StructField::new("status", AttributeType::String).required()],
    };

    let mut map = IndexMap::new();
    map.insert("status".to_string(), Value::String("Enabled".to_string()));
    let single_list = Value::List(vec![Value::Map(map)]);
    let result = struct_type.validate(&single_list);
    assert!(result.is_err());
    let err = result.unwrap_err();
    match &err {
        TypeError::BlockSyntaxNotAllowed { attribute } => {
            assert_eq!(attribute, "VersioningConfiguration");
        }
        other => panic!("Expected BlockSyntaxNotAllowed, got: {:?}", other),
    }
    assert!(
        err.to_string()
            .contains("cannot use block syntax; use map assignment")
    );
}

#[test]
fn struct_rejects_block_syntax_multiple_elements() {
    // Multiple blocks for a bare Struct attribute should also be rejected
    let struct_type = AttributeType::Struct {
        name: "VersioningConfiguration".to_string(),
        fields: vec![StructField::new("status", AttributeType::String).required()],
    };

    let mut map1 = IndexMap::new();
    map1.insert("status".to_string(), Value::String("Enabled".to_string()));
    let mut map2 = IndexMap::new();
    map2.insert("status".to_string(), Value::String("Suspended".to_string()));
    let multi_list = Value::List(vec![Value::Map(map1), Value::Map(map2)]);
    let result = struct_type.validate(&multi_list);
    assert!(result.is_err());
    match result.unwrap_err() {
        TypeError::BlockSyntaxNotAllowed { attribute } => {
            assert_eq!(attribute, "VersioningConfiguration");
        }
        other => panic!("Expected BlockSyntaxNotAllowed, got: {:?}", other),
    }
}

#[test]
fn validate_ipv4_cidr_type() {
    let t = types::ipv4_cidr();

    // Valid IPv4 CIDRs
    assert!(
        t.validate(&Value::String("10.0.0.0/16".to_string()))
            .is_ok()
    );
    assert!(t.validate(&Value::String("0.0.0.0/0".to_string())).is_ok());
    assert!(
        t.validate(&Value::String("255.255.255.255/32".to_string()))
            .is_ok()
    );

    // Invalid IPv4 CIDRs
    assert!(
        t.validate(&Value::String("10.0.0.0/33".to_string()))
            .is_err()
    );
    assert!(t.validate(&Value::String("10.0.0.0".to_string())).is_err());
    assert!(t.validate(&Value::Int(42)).is_err());
}

#[test]
fn validate_ipv6_cidr_type() {
    let t = types::ipv6_cidr();

    // Valid IPv6 CIDRs
    assert!(t.validate(&Value::String("::/0".to_string())).is_ok());
    assert!(
        t.validate(&Value::String("2001:db8::/32".to_string()))
            .is_ok()
    );
    assert!(t.validate(&Value::String("fe80::/10".to_string())).is_ok());
    assert!(t.validate(&Value::String("::1/128".to_string())).is_ok());
    assert!(
        t.validate(&Value::String(
            "2001:0db8:85a3:0000:0000:8a2e:0370:7334/64".to_string()
        ))
        .is_ok()
    );
    assert!(t.validate(&Value::String("ff00::/8".to_string())).is_ok());

    // Invalid IPv6 CIDRs
    assert!(
        t.validate(&Value::String("2001:db8::/129".to_string()))
            .is_err()
    ); // prefix > 128
    assert!(
        t.validate(&Value::String("2001:db8::".to_string()))
            .is_err()
    ); // missing prefix
    assert!(
        t.validate(&Value::String("2001:gggg::/32".to_string()))
            .is_err()
    ); // invalid hex
    assert!(
        t.validate(&Value::String("2001:db8::1::2/64".to_string()))
            .is_err()
    ); // double ::
    assert!(
        t.validate(&Value::String("10.0.0.0/16".to_string()))
            .is_err()
    ); // IPv4, not IPv6
    assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
}

#[test]
fn validate_ipv6_cidr_function_directly() {
    // Valid
    assert!(validate_ipv6_cidr("::/0").is_ok());
    assert!(validate_ipv6_cidr("2001:db8::/32").is_ok());
    assert!(validate_ipv6_cidr("fe80::/10").is_ok());
    assert!(validate_ipv6_cidr("::1/128").is_ok());
    assert!(validate_ipv6_cidr("2001:0db8:85a3:0000:0000:8a2e:0370:7334/64").is_ok());

    // Invalid
    assert!(validate_ipv6_cidr("2001:db8::/129").is_err());
    assert!(validate_ipv6_cidr("not-a-cidr").is_err());
    assert!(validate_ipv6_cidr("2001:db8::").is_err());
    assert!(validate_ipv6_cidr("/64").is_err());
}

#[test]
fn validate_cidr_accepts_both_ipv4_and_ipv6() {
    let t = types::cidr();

    // Valid IPv4 CIDRs
    assert!(
        t.validate(&Value::String("10.0.0.0/16".to_string()))
            .is_ok()
    );
    assert!(t.validate(&Value::String("0.0.0.0/0".to_string())).is_ok());

    // Valid IPv6 CIDRs
    assert!(
        t.validate(&Value::String("2001:db8::/32".to_string()))
            .is_ok()
    );
    assert!(t.validate(&Value::String("::/0".to_string())).is_ok());

    // Invalid
    assert!(
        t.validate(&Value::String("not-a-cidr".to_string()))
            .is_err()
    );
    assert!(t.validate(&Value::String("10.0.0.0".to_string())).is_err()); // no prefix
    assert!(t.validate(&Value::Int(42)).is_err());
}

#[test]
fn custom_type_accepts_resource_ref() {
    // ResourceRef values resolve to strings at runtime, so Custom types should accept them
    let ipv4 = types::ipv4_cidr();
    assert!(
        ipv4.validate(&Value::resource_ref(
            "vpc".to_string(),
            "cidr_block".to_string(),
            vec![]
        ))
        .is_ok()
    );

    let ipv6 = types::ipv6_cidr();
    assert!(
        ipv6.validate(&Value::resource_ref(
            "subnet".to_string(),
            "ipv6_cidr".to_string(),
            vec![]
        ))
        .is_ok()
    );
}

#[test]
fn validate_ipv4_address_type() {
    let t = types::ipv4_address();

    // Valid IPv4 addresses
    assert!(t.validate(&Value::String("10.0.1.5".to_string())).is_ok());
    assert!(
        t.validate(&Value::String("192.168.0.1".to_string()))
            .is_ok()
    );
    assert!(t.validate(&Value::String("0.0.0.0".to_string())).is_ok());
    assert!(
        t.validate(&Value::String("255.255.255.255".to_string()))
            .is_ok()
    );

    // Invalid IPv4 addresses
    assert!(
        t.validate(&Value::String("10.0.0.0/16".to_string()))
            .is_err()
    ); // CIDR, not address
    assert!(t.validate(&Value::String("256.0.0.1".to_string())).is_err()); // octet > 255
    assert!(t.validate(&Value::String("10.0.1".to_string())).is_err()); // only 3 octets
    assert!(t.validate(&Value::String("not-an-ip".to_string())).is_err());
    assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
}

#[test]
fn validate_ipv6_address_type() {
    let t = types::ipv6_address();

    // Valid IPv6 addresses
    assert!(t.validate(&Value::String("::1".to_string())).is_ok());
    assert!(
        t.validate(&Value::String("2001:db8::1".to_string()))
            .is_ok()
    );
    assert!(t.validate(&Value::String("fe80::1".to_string())).is_ok());
    assert!(
        t.validate(&Value::String(
            "2001:0db8:85a3:0000:0000:8a2e:0370:7334".to_string()
        ))
        .is_ok()
    );

    // Invalid IPv6 addresses
    assert!(
        t.validate(&Value::String("2001:db8::/32".to_string()))
            .is_err()
    ); // CIDR, not address
    assert!(t.validate(&Value::String("not-an-ip".to_string())).is_err());
    assert!(t.validate(&Value::String("".to_string())).is_err());
    assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
}

#[test]
fn types_module_has_no_aws_specific_types() {
    // Verify that AWS-specific types are not defined in carina-core.
    // These belong in provider crates (e.g., carina-provider-awscc).
    let source = include_str!("mod.rs");
    let aws_keywords = [
        "fn arn()",
        "fn aws_resource_id()",
        "fn availability_zone()",
        "validate_arn",
        "validate_aws_resource_id",
        "validate_availability_zone",
    ];
    for keyword in &aws_keywords {
        // Exclude this test function itself from the check
        let occurrences: Vec<_> = source.match_indices(keyword).collect();
        // Each keyword appears once in the aws_keywords array literal above
        // If it appears more than once, it means it's also defined elsewhere
        assert!(
            occurrences.len() <= 1,
            "Found AWS-specific type '{}' in carina-core/src/schema.rs. \
             AWS-specific types belong in provider crates.",
            keyword
        );
    }
}

#[test]
fn resource_validator_called() {
    fn my_validator(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
        if attributes.contains_key("forbidden") {
            Err(vec![TypeError::ValidationFailed {
                message: "forbidden attribute not allowed".to_string(),
            }])
        } else {
            Ok(())
        }
    }

    let schema = ResourceSchema::new("test")
        .attribute(AttributeSchema::new("name", AttributeType::String))
        .attribute(AttributeSchema::new("forbidden", AttributeType::String))
        .with_validator(my_validator);

    // Valid: no forbidden attribute
    let mut attrs = HashMap::new();
    attrs.insert("name".to_string(), Value::String("test".to_string()));
    assert!(schema.validate(&attrs).is_ok());

    // Invalid: forbidden attribute present
    let mut bad_attrs = HashMap::new();
    bad_attrs.insert("forbidden".to_string(), Value::String("bad".to_string()));
    let result = schema.validate(&bad_attrs);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().len(), 1);
}

#[test]
fn validate_exclusive_required_helper() {
    use validators::validate_exclusive_required;

    // Valid: exactly one field present
    let mut attrs = HashMap::new();
    attrs.insert("option_a".to_string(), Value::String("value".to_string()));
    assert!(validate_exclusive_required(&attrs, &["option_a", "option_b"]).is_ok());

    let mut attrs2 = HashMap::new();
    attrs2.insert("option_b".to_string(), Value::String("value".to_string()));
    assert!(validate_exclusive_required(&attrs2, &["option_a", "option_b"]).is_ok());

    // Invalid: neither field present
    let empty = HashMap::new();
    let result = validate_exclusive_required(&empty, &["option_a", "option_b"]);
    assert!(result.is_err());
    let errors = result.unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .to_string()
            .contains("Exactly one of [option_a, option_b] must be specified")
    );

    // Invalid: both fields present
    let mut both = HashMap::new();
    both.insert("option_a".to_string(), Value::String("a".to_string()));
    both.insert("option_b".to_string(), Value::String("b".to_string()));
    let result = validate_exclusive_required(&both, &["option_a", "option_b"]);
    assert!(result.is_err());
    let errors = result.unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .to_string()
            .contains("Only one of [option_a, option_b] can be specified")
    );
    assert!(errors[0].to_string().contains("option_a, option_b"));
}

#[test]
fn exclusive_required_with_resource_schema() {
    fn subnet_validator(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
        validators::validate_exclusive_required(attributes, &["cidr_block", "ipv4_ipam_pool_id"])
    }

    let schema = ResourceSchema::new("subnet")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new(
            "ipv4_ipam_pool_id",
            AttributeType::String,
        ))
        .attribute(AttributeSchema::new("vpc_id", AttributeType::String).required())
        .with_validator(subnet_validator);

    // Valid: has cidr_block only
    let mut attrs1 = HashMap::new();
    attrs1.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
    attrs1.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/24".to_string()),
    );
    assert!(schema.validate(&attrs1).is_ok());

    // Valid: has ipv4_ipam_pool_id only
    let mut attrs2 = HashMap::new();
    attrs2.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
    attrs2.insert(
        "ipv4_ipam_pool_id".to_string(),
        Value::String("ipam-pool-123".to_string()),
    );
    assert!(schema.validate(&attrs2).is_ok());

    // Invalid: has neither
    let mut attrs3 = HashMap::new();
    attrs3.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
    let result = schema.validate(&attrs3);
    assert!(result.is_err());

    // Invalid: has both
    let mut attrs4 = HashMap::new();
    attrs4.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
    attrs4.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/24".to_string()),
    );
    attrs4.insert(
        "ipv4_ipam_pool_id".to_string(),
        Value::String("ipam-pool-123".to_string()),
    );
    let result = schema.validate(&attrs4);
    assert!(result.is_err());
}

#[test]
fn exclusive_required_declarative() {
    // Same semantics as the closure-based form above, but declared as data
    // so it can cross the WASM plugin boundary.
    let schema = ResourceSchema::new("vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new(
            "ipv4_ipam_pool_id",
            AttributeType::String,
        ))
        .exclusive_required(&["cidr_block", "ipv4_ipam_pool_id"]);

    // Valid: exactly one present
    let mut one = HashMap::new();
    one.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    assert!(schema.validate(&one).is_ok());

    // Invalid: neither present
    let empty = HashMap::new();
    let err = schema.validate(&empty).unwrap_err();
    assert!(
        err.iter().any(|e| e
            .to_string()
            .contains("Exactly one of [cidr_block, ipv4_ipam_pool_id] must be specified")),
        "missing expected error, got: {:?}",
        err
    );

    // Invalid: both present
    let mut both = HashMap::new();
    both.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    both.insert(
        "ipv4_ipam_pool_id".to_string(),
        Value::String("pool-1".to_string()),
    );
    let err = schema.validate(&both).unwrap_err();
    assert!(
        err.iter().any(|e| e
            .to_string()
            .contains("Only one of [cidr_block, ipv4_ipam_pool_id] can be specified")),
        "missing expected error, got: {:?}",
        err
    );
}

#[test]
fn exclusive_required_multiple_groups() {
    let schema = ResourceSchema::new("multi")
        .attribute(AttributeSchema::new("a", AttributeType::String))
        .attribute(AttributeSchema::new("b", AttributeType::String))
        .attribute(AttributeSchema::new("x", AttributeType::String))
        .attribute(AttributeSchema::new("y", AttributeType::String))
        .exclusive_required(&["a", "b"])
        .exclusive_required(&["x", "y"]);

    // Neither group satisfied → two errors
    let err = schema.validate(&HashMap::new()).unwrap_err();
    assert_eq!(
        err.iter()
            .filter(|e| e.to_string().contains("Exactly one of"))
            .count(),
        2,
        "expected two missing-group errors, got: {:?}",
        err
    );

    // Satisfy both groups
    let mut ok = HashMap::new();
    ok.insert("a".to_string(), Value::String("1".to_string()));
    ok.insert("x".to_string(), Value::String("1".to_string()));
    assert!(schema.validate(&ok).is_ok());
}

#[test]
fn validate_union_type() {
    // Create two Custom types that validate different prefixes
    let type_a = AttributeType::Custom {
        semantic_name: Some("TypeA".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |value| {
            if let Value::String(s) = value {
                if s.starts_with("a-") {
                    Ok(())
                } else {
                    Err(format!("Expected 'a-' prefix, got '{}'", s))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    };
    let type_b = AttributeType::Custom {
        semantic_name: Some("TypeB".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |value| {
            if let Value::String(s) = value {
                if s.starts_with("b-") {
                    Ok(())
                } else {
                    Err(format!("Expected 'b-' prefix, got '{}'", s))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
        to_dsl: None,
    };

    let union_type = AttributeType::Union(vec![type_a, type_b]);

    // Valid: matches first member
    assert!(
        union_type
            .validate(&Value::String("a-12345678".to_string()))
            .is_ok()
    );
    // Valid: matches second member
    assert!(
        union_type
            .validate(&Value::String("b-12345678".to_string()))
            .is_ok()
    );
    // Invalid: matches neither
    assert!(
        union_type
            .validate(&Value::String("c-12345678".to_string()))
            .is_err()
    );
    // Valid: ResourceRef is accepted by Custom members
    assert!(
        union_type
            .validate(&Value::resource_ref(
                "gw".to_string(),
                "id".to_string(),
                vec![]
            ))
            .is_ok()
    );
}

#[test]
fn union_struct_unknown_field_shows_specific_error() {
    let principal_type = AttributeType::Union(vec![
        AttributeType::Struct {
            name: "Principal".to_string(),
            fields: vec![
                StructField::new("service", AttributeType::String),
                StructField::new("federated", AttributeType::String),
            ],
        },
        AttributeType::String,
    ]);

    let mut map = IndexMap::new();
    map.insert(
        "federated".to_string(),
        Value::String("arn:...".to_string()),
    );
    map.insert("aaa".to_string(), Value::String("bbb".to_string()));

    let err = principal_type.validate(&Value::Map(map)).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("aaa"),
        "Error should mention unknown field 'aaa'. Got: {msg}"
    );
    assert!(
        msg.contains("Principal"),
        "Error should mention struct name. Got: {msg}"
    );
}

#[test]
fn union_type_name() {
    let type_a = AttributeType::Custom {
        semantic_name: Some("TypeA".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    let type_b = AttributeType::Custom {
        semantic_name: Some("TypeB".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };

    let union_type = AttributeType::Union(vec![type_a, type_b]);
    assert_eq!(union_type.type_name(), "TypeA | TypeB");
}

#[test]
fn union_accepts_type_name() {
    let type_a = AttributeType::Custom {
        semantic_name: Some("TypeA".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    let type_b = AttributeType::Custom {
        semantic_name: Some("TypeB".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };

    let union_type = AttributeType::Union(vec![type_a, type_b]);
    assert!(union_type.accepts_type_name("TypeA"));
    assert!(union_type.accepts_type_name("TypeB"));
    assert!(!union_type.accepts_type_name("TypeC"));

    // Non-union types
    let simple = AttributeType::String;
    assert!(simple.accepts_type_name("String"));
    assert!(!simple.accepts_type_name("Int"));
}

#[test]
fn with_block_name_builder() {
    let attr = AttributeSchema::new("operating_regions", AttributeType::String)
        .with_block_name("operating_region");
    assert_eq!(attr.block_name.as_deref(), Some("operating_region"));
}

#[test]
fn block_name_default_is_none() {
    let attr = AttributeSchema::new("name", AttributeType::String);
    assert!(attr.block_name.is_none());
}

#[test]
fn block_name_map_returns_mapping() {
    let schema = ResourceSchema::new("test.resource")
        .attribute(
            AttributeSchema::new("operating_regions", AttributeType::String)
                .with_block_name("operating_region"),
        )
        .attribute(AttributeSchema::new("name", AttributeType::String));

    let map = schema.block_name_map();
    assert_eq!(map.len(), 1);
    assert_eq!(map.get("operating_region").unwrap(), "operating_regions");
}

#[test]
fn block_name_map_empty_when_no_block_names() {
    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::new("name", AttributeType::String));

    let map = schema.block_name_map();
    assert!(map.is_empty());
}

#[test]
fn resolve_block_names_renames_key() {
    let mut resources = vec![{
        let mut r = Resource::new("ec2.ipam", "my-ipam");
        // Block syntax produces Value::List
        r.set_attr(
            "operating_region".to_string(),
            Value::List(vec![Value::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "region_name".to_string(),
                    Value::String("us-east-1".to_string()),
                );
                m
            })]),
        );
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.ipam".to_string(),
        ResourceSchema::new("ec2.ipam").attribute(
            AttributeSchema::new("operating_regions", AttributeType::String)
                .with_block_name("operating_region"),
        ),
    );

    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    assert!(resources[0].attributes.contains_key("operating_regions"));
    assert!(!resources[0].attributes.contains_key("operating_region"));
}

#[test]
fn resolve_block_names_noop_when_no_match() {
    let mut resources = vec![{
        let mut r = Resource::new("ec2.ipam", "my-ipam");
        r.set_attr("name".to_string(), Value::String("test".to_string()));
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.ipam".to_string(),
        ResourceSchema::new("ec2.ipam")
            .attribute(AttributeSchema::new("name", AttributeType::String)),
    );

    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    assert!(resources[0].attributes.contains_key("name"));
}

#[test]
fn resolve_block_names_errors_on_mixed_syntax() {
    let mut resources = vec![{
        let mut r = Resource::new("ec2.ipam", "my-ipam");
        // Block syntax produces Value::List
        r.set_attr(
            "operating_region".to_string(),
            Value::List(vec![Value::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "region_name".to_string(),
                    Value::String("us-east-1".to_string()),
                );
                m
            })]),
        );
        // User also explicitly set the canonical name
        r.set_attr(
            "operating_regions".to_string(),
            Value::List(vec![Value::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "region_name".to_string(),
                    Value::String("us-west-2".to_string()),
                );
                m
            })]),
        );
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.ipam".to_string(),
        ResourceSchema::new("ec2.ipam").attribute(
            AttributeSchema::new("operating_regions", AttributeType::String)
                .with_block_name("operating_region"),
        ),
    );

    let result = resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone());
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("operating_region"));
    assert!(err.contains("operating_regions"));
}

#[test]
fn resolve_block_names_skips_unknown_schema() {
    let mut resources = vec![{
        let mut r = Resource::new("unknown.type", "test");
        r.set_attr(
            "operating_region".to_string(),
            Value::String("us-east-1".to_string()),
        );
        r
    }];

    let schemas = HashMap::new();

    // Should not error for unknown resource types
    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    // Key should remain unchanged
    assert!(resources[0].attributes.contains_key("operating_region"));
}

#[test]
fn struct_field_with_block_name() {
    let field = StructField::new(
        "transitions",
        AttributeType::list(AttributeType::Struct {
            name: "Transition".to_string(),
            fields: vec![],
        }),
    )
    .with_block_name("transition");
    assert_eq!(field.block_name.as_deref(), Some("transition"));
}

#[test]
fn resolve_block_names_nested_struct() {
    // Simulate: lifecycle_configuration = { transition { ... } }
    // where "transition" is the block name for "transitions" field
    let mut inner_map = IndexMap::new();
    inner_map.insert(
        "transition".to_string(),
        Value::List(vec![Value::Map({
            let mut m = IndexMap::new();
            m.insert(
                "storage_class".to_string(),
                Value::String("GLACIER".to_string()),
            );
            m
        })]),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr("lifecycle_configuration".to_string(), Value::Map(inner_map));
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
        ResourceSchema::new("s3.Bucket").attribute(AttributeSchema::new(
            "lifecycle_configuration",
            AttributeType::Struct {
                name: "LifecycleConfiguration".to_string(),
                fields: vec![
                    StructField::new(
                        "transitions",
                        AttributeType::list(AttributeType::Struct {
                            name: "Transition".to_string(),
                            fields: vec![],
                        }),
                    )
                    .with_block_name("transition"),
                ],
            },
        )),
    );

    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    // The nested "transition" key should be renamed to "transitions"
    let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
        Some(Value::Map(m)) => m,
        _ => panic!("expected Map"),
    };
    assert!(
        lifecycle.contains_key("transitions"),
        "expected 'transitions' key after resolve"
    );
    assert!(
        !lifecycle.contains_key("transition"),
        "expected 'transition' key to be removed"
    );
}

#[test]
fn resolve_block_names_singular_field_not_renamed_when_assigned() {
    // When a struct has both `transition` (Struct) and `transitions` (List(Struct))
    // with block_name("transition") on the List field, an attribute assignment
    // `transition = { ... }` (Value::Map) should NOT be renamed to `transitions`.
    // Only block syntax `transition { ... }` (Value::List) should be renamed.
    let mut inner_map = IndexMap::new();
    // This is an attribute assignment: transition = { storage_class = "GLACIER" }
    // Parser produces Value::Map for attribute assignments
    inner_map.insert(
        "transition".to_string(),
        Value::Map({
            let mut m = IndexMap::new();
            m.insert(
                "storage_class".to_string(),
                Value::String("GLACIER".to_string()),
            );
            m
        }),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr("lifecycle_configuration".to_string(), Value::Map(inner_map));
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
        ResourceSchema::new("s3.Bucket").attribute(AttributeSchema::new(
            "lifecycle_configuration",
            AttributeType::Struct {
                name: "LifecycleConfiguration".to_string(),
                fields: vec![
                    StructField::new(
                        "transition",
                        AttributeType::Struct {
                            name: "Transition".to_string(),
                            fields: vec![],
                        },
                    ),
                    StructField::new(
                        "transitions",
                        AttributeType::list(AttributeType::Struct {
                            name: "Transition".to_string(),
                            fields: vec![],
                        }),
                    )
                    .with_block_name("transition"),
                ],
            },
        )),
    );

    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
        Some(Value::Map(m)) => m,
        _ => panic!("expected Map"),
    };
    // The Value::Map should remain as "transition" (not renamed)
    assert!(
        lifecycle.contains_key("transition"),
        "expected 'transition' key to remain (attribute assignment)"
    );
    assert!(
        !lifecycle.contains_key("transitions"),
        "expected 'transitions' key NOT to be created from attribute assignment"
    );
}

#[test]
fn resolve_block_names_block_syntax_renamed_when_singular_field_exists() {
    // Block syntax `transition { ... }` should still be renamed to `transitions`
    // even when a singular `transition` field exists in the schema.
    let mut inner_map = IndexMap::new();
    // Block syntax produces Value::List
    inner_map.insert(
        "transition".to_string(),
        Value::List(vec![Value::Map({
            let mut m = IndexMap::new();
            m.insert(
                "storage_class".to_string(),
                Value::String("GLACIER".to_string()),
            );
            m
        })]),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr("lifecycle_configuration".to_string(), Value::Map(inner_map));
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "s3.Bucket".to_string(),
        ResourceSchema::new("s3.Bucket").attribute(AttributeSchema::new(
            "lifecycle_configuration",
            AttributeType::Struct {
                name: "LifecycleConfiguration".to_string(),
                fields: vec![
                    StructField::new(
                        "transition",
                        AttributeType::Struct {
                            name: "Transition".to_string(),
                            fields: vec![],
                        },
                    ),
                    StructField::new(
                        "transitions",
                        AttributeType::list(AttributeType::Struct {
                            name: "Transition".to_string(),
                            fields: vec![],
                        }),
                    )
                    .with_block_name("transition"),
                ],
            },
        )),
    );

    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
        Some(Value::Map(m)) => m,
        _ => panic!("expected Map"),
    };
    // Block syntax (Value::List) should be renamed to "transitions"
    assert!(
        lifecycle.contains_key("transitions"),
        "expected 'transitions' key after resolve (block syntax)"
    );
    assert!(
        !lifecycle.contains_key("transition"),
        "expected 'transition' key to be removed (block syntax renamed)"
    );
}

#[test]
fn resolve_block_names_same_block_and_canonical_name() {
    // When block_name == canonical attribute name, block syntax should work
    // without triggering a false "cannot use both" error.
    // This regression was introduced in PR #913 and fixed in PR #917.
    let mut resources = vec![{
        let mut r = Resource::new("ec2.SecurityGroup", "my-sg");
        // Block syntax produces Value::List
        r.set_attr(
            "ingress".to_string(),
            Value::List(vec![Value::Map({
                let mut m = IndexMap::new();
                m.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
                m
            })]),
        );
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.SecurityGroup".to_string(),
        ResourceSchema::new("ec2.SecurityGroup").attribute(
            AttributeSchema::new(
                "ingress",
                AttributeType::list(AttributeType::Struct {
                    name: "Ingress".to_string(),
                    fields: vec![StructField::new("ip_protocol", AttributeType::String)],
                }),
            )
            .with_block_name("ingress"),
        ),
    );

    // Should succeed without errors (block_name == canonical name, no rename needed)
    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    // Key should remain as "ingress"
    assert!(resources[0].attributes.contains_key("ingress"));
    // Value should be unchanged
    match resources[0].get_attr("ingress") {
        Some(Value::List(items)) => assert_eq!(items.len(), 1),
        other => panic!("expected List, got {:?}", other),
    }
}

#[test]
fn resolve_block_names_same_block_and_canonical_name_multiple_items() {
    // When block_name == canonical name and the user provides multiple block
    // items (Value::List with multiple entries), no conflict should occur.
    // The key already exists (it IS the canonical key), so the `continue`
    // path handles it. This test verifies all items are preserved.
    let mut resources = vec![{
        let mut r = Resource::new("ec2.SecurityGroup", "my-sg");
        r.set_attr(
            "ingress".to_string(),
            Value::List(vec![
                Value::Map({
                    let mut m = IndexMap::new();
                    m.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
                    m
                }),
                Value::Map({
                    let mut m = IndexMap::new();
                    m.insert("ip_protocol".to_string(), Value::String("udp".to_string()));
                    m
                }),
            ]),
        );
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "ec2.SecurityGroup".to_string(),
        ResourceSchema::new("ec2.SecurityGroup").attribute(
            AttributeSchema::new(
                "ingress",
                AttributeType::list(AttributeType::Struct {
                    name: "Ingress".to_string(),
                    fields: vec![StructField::new("ip_protocol", AttributeType::String)],
                }),
            )
            .with_block_name("ingress"),
        ),
    );

    // Should succeed; block_name == canonical name means no conflict possible
    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    assert!(resources[0].attributes.contains_key("ingress"));
    match resources[0].get_attr("ingress") {
        Some(Value::List(items)) => assert_eq!(items.len(), 2),
        other => panic!("expected List with 2 items, got {:?}", other),
    }
}

#[test]
fn resolve_block_names_nested_same_block_and_canonical_name() {
    // Nested struct field where block_name == canonical field name.
    // Should resolve without errors.
    let mut inner_map = IndexMap::new();
    inner_map.insert(
        "tag".to_string(),
        Value::List(vec![Value::Map({
            let mut m = IndexMap::new();
            m.insert("key".to_string(), Value::String("Name".to_string()));
            m.insert("value".to_string(), Value::String("test".to_string()));
            m
        })]),
    );

    let mut resources = vec![{
        let mut r = Resource::new("test.resource", "my-resource");
        r.set_attr("config".to_string(), Value::Map(inner_map));
        r
    }];

    let mut schemas = HashMap::new();
    schemas.insert(
        "test.resource".to_string(),
        ResourceSchema::new("test.resource").attribute(AttributeSchema::new(
            "config",
            AttributeType::Struct {
                name: "Config".to_string(),
                fields: vec![
                    StructField::new(
                        "tag",
                        AttributeType::list(AttributeType::Struct {
                            name: "Tag".to_string(),
                            fields: vec![
                                StructField::new("key", AttributeType::String),
                                StructField::new("value", AttributeType::String),
                            ],
                        }),
                    )
                    .with_block_name("tag"),
                ],
            },
        )),
    );

    // Should succeed without errors
    resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

    let config = match resources[0].get_attr("config") {
        Some(Value::Map(m)) => m,
        _ => panic!("expected Map"),
    };
    // Key should remain as "tag" (no rename needed since block_name == canonical)
    assert!(
        config.contains_key("tag"),
        "expected 'tag' key to remain (block_name == canonical name)"
    );
    match config.get("tag") {
        Some(Value::List(items)) => assert_eq!(items.len(), 1),
        other => panic!("expected List, got {:?}", other),
    }
}

#[test]
fn test_operation_config_default() {
    let config = OperationConfig::default();
    assert_eq!(config.delete_timeout_secs, None);
    assert_eq!(config.delete_max_retries, None);
    assert_eq!(config.create_timeout_secs, None);
    assert_eq!(config.create_max_retries, None);
}

#[test]
fn test_resource_schema_with_operation_config() {
    let schema =
        ResourceSchema::new("ec2.transit_gateway").with_operation_config(OperationConfig {
            delete_timeout_secs: Some(1800),
            delete_max_retries: Some(24),
            ..Default::default()
        });
    let config = schema.operation_config.unwrap();
    assert_eq!(config.delete_timeout_secs, Some(1800));
    assert_eq!(config.delete_max_retries, Some(24));
    assert_eq!(config.create_timeout_secs, None);
}

#[test]
fn test_resource_schema_without_operation_config() {
    let schema = ResourceSchema::new("ec2.Vpc");
    assert!(schema.operation_config.is_none());
}

#[test]
fn validate_rejects_unknown_attribute() {
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String));

    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::String("my-bucket".to_string()),
    );
    attrs.insert("tags".to_string(), Value::Map(IndexMap::new()));

    let result = schema.validate(&attrs);
    assert!(result.is_err());
    let errors = result.unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(matches!(&errors[0], TypeError::UnknownAttribute { name, .. } if name == "tags"));
}

#[test]
fn validate_allows_known_attributes_only() {
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String))
        .attribute(AttributeSchema::new(
            "tags",
            AttributeType::map(AttributeType::String),
        ));

    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::String("my-bucket".to_string()),
    );
    attrs.insert("tags".to_string(), Value::Map(IndexMap::new()));

    assert!(schema.validate(&attrs).is_ok());
}

#[test]
fn validate_unknown_attribute_with_suggestion() {
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String));

    let mut attrs = HashMap::new();
    attrs.insert(
        "bukcet_name".to_string(),
        Value::String("my-bucket".to_string()),
    );

    let result = schema.validate(&attrs);
    assert!(result.is_err());
    let errors = result.unwrap_err();
    assert_eq!(errors.len(), 1);
    match &errors[0] {
        TypeError::UnknownAttribute { name, suggestion } => {
            assert_eq!(name, "bukcet_name");
            assert_eq!(suggestion.as_deref(), Some("bucket_name"));
        }
        other => panic!("Expected UnknownAttribute, got: {:?}", other),
    }
}

#[test]
fn validate_accepts_block_name_alias() {
    let schema = ResourceSchema::new("ec2.SecurityGroup").attribute(
        AttributeSchema::new(
            "ingress_rules",
            AttributeType::List {
                inner: Box::new(AttributeType::String),
                ordered: false,
            },
        )
        .with_block_name("ingress_rule"),
    );

    let mut attrs = HashMap::new();
    attrs.insert(
        "ingress_rule".to_string(),
        Value::List(vec![Value::String("rule1".to_string())]),
    );

    assert!(schema.validate(&attrs).is_ok());
}

#[test]
fn validate_skips_internal_attributes() {
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String));

    let mut attrs = HashMap::new();
    attrs.insert(
        "bucket_name".to_string(),
        Value::String("my-bucket".to_string()),
    );
    attrs.insert("_binding".to_string(), Value::String("b".to_string()));

    assert!(schema.validate(&attrs).is_ok());
}

fn make_custom(name: &str, base: AttributeType) -> AttributeType {
    AttributeType::Custom {
        semantic_name: Some(name.to_string()),
        base: Box::new(base),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    }
}

fn make_custom_anon_pattern(pattern: &str) -> AttributeType {
    AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: Some(pattern.to_string()),
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    }
}

fn make_custom_anon_len(min: u64, max: u64) -> AttributeType {
    AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: None,
        length: Some((Some(min), Some(max))),
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    }
}

#[test]
fn assignable_allows_same_primitives() {
    assert!(AttributeType::String.is_assignable_to(&AttributeType::String));
    assert!(AttributeType::Int.is_assignable_to(&AttributeType::Int));
    assert!(!AttributeType::Int.is_assignable_to(&AttributeType::String));
    assert!(!AttributeType::Bool.is_assignable_to(&AttributeType::Int));
}

#[test]
fn assignable_rejects_distinct_semantic_names() {
    let vpc = make_custom("VpcId", AttributeType::String);
    let subnet = make_custom("SubnetId", AttributeType::String);
    assert!(!vpc.is_assignable_to(&subnet));
    assert!(!subnet.is_assignable_to(&vpc));
}

#[test]
fn assignable_allows_same_semantic_name() {
    let a = make_custom("VpcId", AttributeType::String);
    let b = make_custom("VpcId", AttributeType::String);
    assert!(a.is_assignable_to(&b));
}

#[test]
fn assignable_narrow_to_anonymous_unconstrained_sink() {
    // Semantic source with no pattern assigns to fully-anonymous unconstrained sink.
    let account = make_custom("AwsAccountId", AttributeType::String);
    let anon = AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    assert!(account.is_assignable_to(&anon));
}

#[test]
fn assignable_source_without_pattern_rejected_by_patterned_sink() {
    let account = make_custom("AwsAccountId", AttributeType::String);
    let anon = make_custom_anon_pattern("^\\d{12}$");
    // Source has no pattern; sink demands one → NG.
    assert!(!account.is_assignable_to(&anon));
}

#[test]
fn assignable_anon_to_anon_length_containment() {
    let narrow = make_custom_anon_len(1, 36);
    let wide = make_custom_anon_len(1, 64);
    assert!(narrow.is_assignable_to(&wide));
    assert!(!wide.is_assignable_to(&narrow));
}

#[test]
fn assignable_rejects_non_custom_to_custom() {
    let vpc = make_custom("VpcId", AttributeType::String);
    assert!(!AttributeType::String.is_assignable_to(&vpc));
}

#[test]
fn assignable_custom_to_non_custom_recurses_on_base() {
    // AwsAccountId (base: String) assigns to a plain String sink.
    let account = make_custom("AwsAccountId", AttributeType::String);
    assert!(account.is_assignable_to(&AttributeType::String));
}

#[test]
fn assignable_union_sink_accepts_assignable_member() {
    let vpc = make_custom("VpcId", AttributeType::String);
    let other_vpc = make_custom("VpcId", AttributeType::String);
    let union = AttributeType::Union(vec![vpc, AttributeType::String]);
    assert!(other_vpc.is_assignable_to(&union));
}

#[test]
fn assignable_union_source_requires_all_members_assignable() {
    // All members of source must be assignable to sink.
    let vpc = make_custom("VpcId", AttributeType::String);
    let anon_any = AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    let both_ok = AttributeType::Union(vec![vpc.clone(), vpc.clone()]);
    assert!(both_ok.is_assignable_to(&vpc));

    let subnet = make_custom("SubnetId", AttributeType::String);
    let mixed = AttributeType::Union(vec![vpc.clone(), subnet]);
    // One member (SubnetId) not assignable to VpcId sink → whole union NG.
    assert!(!mixed.is_assignable_to(&vpc));

    // But is assignable to an anonymous unconstrained sink.
    assert!(mixed.is_assignable_to(&anon_any));
}

#[test]
fn semantic_custom_assigns_to_anonymous_unconstrained_sink() {
    // Replaces the old buggy `is_compatible_with_two_string_based_customs`
    // which asserted VpcId <-> SubnetId were symmetric-compatible.
    let vpc = make_custom("VpcId", AttributeType::String);
    let anon = AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    assert!(vpc.is_assignable_to(&anon));
    // Reverse: anon has no proof it's a VpcId → NG.
    assert!(!anon.is_assignable_to(&vpc));
}

#[test]
fn assignable_int_custom_rejects_string_custom() {
    let int_custom = make_custom("Port", AttributeType::Int);
    let string_custom = make_custom("VpcId", AttributeType::String);
    assert!(!int_custom.is_assignable_to(&string_custom));
}

// -- Custom-pattern and Custom-length assignability matrix (#2218) --
//
// The rules `is_assignable_to` enforces for `Custom { pattern, length, .. }`:
//
// - pattern: differing literal strings are conservatively *incompatible*
//   (we cannot prove a regex is a refinement of another by string compare,
//   so we err on the side of rejecting). `None` on the sink means "no
//   pattern constraint" and admits any source pattern (or none). `None`
//   on the source against a `Some` sink is rejected — the source has no
//   proof its values match the sink's pattern.
// - length: source ⊆ sink (sink.min ≤ source.min AND source.max ≤ sink.max,
//   missing bounds treated as unbounded on that side). `None` on the sink
//   admits any source length; `None` on the source against a `Some` sink
//   is rejected — the source has no proof its values fit the sink range.

fn make_custom_anon_pattern_and_len(
    pattern: Option<&str>,
    length: Option<(Option<u64>, Option<u64>)>,
) -> AttributeType {
    AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: pattern.map(str::to_string),
        length,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    }
}

#[test]
fn assignable_anon_pattern_equal_strings_compatible() {
    let a = make_custom_anon_pattern_and_len(Some("^a+$"), None);
    let b = make_custom_anon_pattern_and_len(Some("^a+$"), None);
    assert!(a.is_assignable_to(&b));
}

#[test]
fn assignable_anon_pattern_differing_strings_incompatible() {
    // Even if both regexes might describe overlapping languages, the
    // implementation does not prove containment — differing pattern
    // strings are conservatively rejected.
    let a = make_custom_anon_pattern_and_len(Some("^a+$"), None);
    let b = make_custom_anon_pattern_and_len(Some("^a*$"), None);
    assert!(!a.is_assignable_to(&b));
    assert!(!b.is_assignable_to(&a));
}

#[test]
fn assignable_anon_pattern_source_none_sink_some_rejected() {
    // Source has no pattern; sink demands one — source has no proof
    // its values match the sink's pattern.
    let source = make_custom_anon_pattern_and_len(None, None);
    let sink = make_custom_anon_pattern_and_len(Some("^x+$"), None);
    assert!(!source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_pattern_source_some_sink_none_compatible() {
    // Sink has no pattern constraint — any source pattern is fine.
    let source = make_custom_anon_pattern_and_len(Some("^x+$"), None);
    let sink = make_custom_anon_pattern_and_len(None, None);
    assert!(source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_source_narrower_compatible() {
    // Source length range ⊂ sink range → compatible.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(20), Some(30))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_source_wider_min_rejected() {
    // Source min < sink min → values shorter than sink allows could leak through.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(5), Some(40))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_source_wider_max_rejected() {
    // Source max > sink max → values longer than sink allows could leak through.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(50))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_source_unbounded_max_against_bounded_sink_rejected() {
    // Source has no upper bound; sink does → source could exceed sink max.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(10), None)));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_source_unbounded_min_against_bounded_sink_rejected() {
    // Source has no lower bound; sink does → source could fall below sink min.
    let source = make_custom_anon_pattern_and_len(None, Some((None, Some(40))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_source_none_sink_some_rejected() {
    // Source has no length constraint at all; sink does → no proof.
    let source = make_custom_anon_pattern_and_len(None, None);
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_source_some_sink_none_compatible() {
    // Sink has no length constraint — any source length is fine.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    let sink = make_custom_anon_pattern_and_len(None, None);
    assert!(source.is_assignable_to(&sink));
}

#[test]
fn assignable_anon_length_both_none_compatible() {
    let source = make_custom_anon_pattern_and_len(None, None);
    let sink = make_custom_anon_pattern_and_len(None, None);
    assert!(source.is_assignable_to(&sink));
}

#[test]
fn custom_carries_semantic_name_pattern_length() {
    let t = AttributeType::Custom {
        semantic_name: Some("VpcId".to_string()),
        base: Box::new(AttributeType::String),
        pattern: Some("^vpc-[a-f0-9]+$".to_string()),
        length: Some((Some(8), Some(21))),
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    match t {
        AttributeType::Custom {
            semantic_name,
            pattern,
            length,
            ..
        } => {
            assert_eq!(semantic_name.as_deref(), Some("VpcId"));
            assert_eq!(pattern.as_deref(), Some("^vpc-[a-f0-9]+$"));
            assert_eq!(length, Some((Some(8), Some(21))));
        }
        _ => panic!("expected Custom"),
    }
}

#[test]
fn custom_type_name_anonymous_pattern_only() {
    let t = AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: Some("^foo$".to_string()),
        length: None,
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    assert_eq!(t.type_name(), "String(pattern)");
}

#[test]
fn custom_type_name_anonymous_length_only() {
    let t = AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: None,
        length: Some((Some(1), Some(64))),
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    assert_eq!(t.type_name(), "String(len: 1..=64)");
}

#[test]
fn custom_type_name_anonymous_pattern_and_length() {
    let t = AttributeType::Custom {
        semantic_name: None,
        base: Box::new(AttributeType::String),
        pattern: Some("^.*$".to_string()),
        length: Some((Some(1), Some(64))),
        validate: |_| Ok(()),
        namespace: None,
        to_dsl: None,
    };
    assert_eq!(t.type_name(), "String(pattern, len: 1..=64)");
}

#[test]
fn validate_email_function_directly() {
    // Valid
    assert!(validate_email("user@example.com").is_ok());
    assert!(validate_email("user.name+tag@sub.example.co.jp").is_ok());
    assert!(validate_email("a@b.c").is_ok());

    // Invalid: no '@'
    assert!(validate_email("no-at-sign.com").is_err());
    // Invalid: no dot in domain
    assert!(validate_email("noTLD@host").is_err());
    // Invalid: empty local-part
    assert!(validate_email("@example.com").is_err());
    // Invalid: empty domain
    assert!(validate_email("user@").is_err());
    // Invalid: empty input
    assert!(validate_email("").is_err());
    // Invalid: more than one '@'
    assert!(validate_email("a@b@c.com").is_err());
    // Invalid: empty domain label (consecutive dots)
    assert!(validate_email("user@example..com").is_err());
    // Invalid: trailing dot in domain creates empty label
    assert!(validate_email("user@example.com.").is_err());
    // Invalid: whitespace
    assert!(validate_email("us er@example.com").is_err());
    assert!(validate_email("user@exa mple.com").is_err());
}

#[test]
fn validate_email_type() {
    let t = types::email();

    // Type identity: Custom with semantic_name "Email" and String base
    match &t {
        AttributeType::Custom {
            semantic_name,
            base,
            ..
        } => {
            assert_eq!(semantic_name.as_deref(), Some("Email"));
            assert!(matches!(**base, AttributeType::String));
        }
        other => panic!("Expected AttributeType::Custom, got: {:?}", other),
    }

    // Valid emails
    assert!(
        t.validate(&Value::String("user@example.com".to_string()))
            .is_ok()
    );
    assert!(
        t.validate(&Value::String(
            "user.name+tag@sub.example.co.jp".to_string()
        ))
        .is_ok()
    );

    // Invalid emails
    assert!(
        t.validate(&Value::String("no-at-sign.com".to_string()))
            .is_err()
    );
    assert!(
        t.validate(&Value::String("noTLD@host".to_string()))
            .is_err()
    );
    assert!(
        t.validate(&Value::String("@example.com".to_string()))
            .is_err()
    );
    assert!(t.validate(&Value::String("user@".to_string())).is_err());
    assert!(t.validate(&Value::String("".to_string())).is_err());

    // Wrong type
    assert!(t.validate(&Value::Int(42)).is_err());
}

#[cfg(test)]
mod validate_collect_tests {
    use super::*;
    use indexmap::IndexMap;

    fn struct_type(name: &str, fields: Vec<StructField>) -> AttributeType {
        AttributeType::Struct {
            name: name.to_string(),
            fields,
        }
    }

    fn map_value(entries: Vec<(&str, Value)>) -> Value {
        let mut map = IndexMap::new();
        for (k, v) in entries {
            map.insert(k.to_string(), v);
        }
        Value::Map(map)
    }

    #[test]
    fn collect_empty_on_valid_value() {
        let ty = struct_type(
            "Versioning",
            vec![
                StructField::new("status", AttributeType::String).required(),
                StructField::new("mfa_delete", AttributeType::Bool),
            ],
        );
        let v = map_value(vec![
            ("status", Value::String("Enabled".to_string())),
            ("mfa_delete", Value::Bool(false)),
        ]);
        let errors = ty.validate_collect(&v);
        assert!(
            errors.is_empty(),
            "valid struct must produce no errors, got {errors:?}"
        );
    }

    #[test]
    fn collect_reports_all_unknown_fields_with_path() {
        // Two unknown fields in one struct — `validate_collect` must
        // surface both, while the legacy `validate()` would stop at the
        // first.
        let ty = struct_type(
            "Versioning",
            vec![StructField::new("status", AttributeType::String)],
        );
        let v = map_value(vec![
            ("statuus", Value::String("Enabled".to_string())),
            ("mfa", Value::Bool(false)),
        ]);
        let errors = ty.validate_collect(&v);
        assert_eq!(
            errors.len(),
            2,
            "expected two unknown-field errors, got {errors:?}"
        );
        let names: Vec<String> = errors.iter().map(|(p, _)| p.to_string()).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(names.contains(&"statuus"), "got {names:?}");
        assert!(names.contains(&"mfa"), "got {names:?}");
    }

    #[test]
    fn collect_descends_into_nested_struct_with_field_path() {
        // Inner struct has a type error — the path must record the
        // outer field name then the inner field name so the LSP can
        // walk back to the source position.
        let inner = struct_type(
            "Inner",
            vec![StructField::new("count", AttributeType::Int).required()],
        );
        let outer = struct_type("Outer", vec![StructField::new("nested", inner).required()]);
        let v = map_value(vec![(
            "nested",
            map_value(vec![("count", Value::String("not an int".to_string()))]),
        )]);
        let errors = outer.validate_collect(&v);
        assert_eq!(errors.len(), 1, "got {errors:?}");
        let (path, _err) = &errors[0];
        let steps: Vec<String> = path.steps().iter().map(|s| s.to_string()).collect();
        assert_eq!(steps, vec!["nested".to_string(), "count".to_string()]);
    }

    #[test]
    fn collect_descends_into_list_of_struct_with_index_path() {
        // List<Struct> errors must include the list index in the path
        // so the LSP can locate the offending block.
        let inner = struct_type(
            "Item",
            vec![StructField::new("name", AttributeType::String).required()],
        );
        let outer_attr = AttributeType::List {
            inner: Box::new(inner),
            ordered: true,
        };

        // Two list items, second one has wrong type for `name`
        let v = Value::List(vec![
            map_value(vec![("name", Value::String("ok".to_string()))]),
            map_value(vec![("name", Value::Int(42))]),
        ]);
        let errors = outer_attr.validate_collect(&v);
        assert_eq!(errors.len(), 1, "got {errors:?}");
        let path = &errors[0].0;
        let steps: Vec<String> = path.steps().iter().map(|s| s.to_string()).collect();
        assert_eq!(steps, vec!["[1]".to_string(), "name".to_string()]);
    }

    #[test]
    fn collect_resolves_block_name_alias() {
        // `block_name` is an alias users can type instead of `name` in
        // the DSL (e.g. `transition` for the canonical `transitions`).
        // The unified validator must recognise the alias rather than
        // flagging it as an unknown field — the LSP used to do this
        // alias lookup itself, but that responsibility now belongs to
        // the core validator.
        let ty = struct_type(
            "Lifecycle",
            vec![
                StructField::new("transitions", AttributeType::String)
                    .with_block_name("transition"),
            ],
        );
        let v = map_value(vec![("transition", Value::String("ok".to_string()))]);
        let errors = ty.validate_collect(&v);
        assert!(
            errors.is_empty(),
            "block_name alias must not flag the field as unknown, got {errors:?}"
        );
    }

    #[test]
    fn collect_skips_resource_ref_in_struct_field() {
        // ResourceRef values are placeholders that resolve at apply
        // time. Skipping them avoids spurious "wrong type" errors for
        // fields whose value is `vpc.id` etc.
        let ty = struct_type(
            "Subnet",
            vec![StructField::new("vpc_id", AttributeType::Int)],
        );
        let v = map_value(vec![(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
        )]);
        let errors = ty.validate_collect(&v);
        assert!(
            errors.is_empty(),
            "ResourceRef in struct field must not produce an error, got {errors:?}"
        );
    }

    #[test]
    fn collect_yields_unknown_struct_field_with_suggestion() {
        // The existing `validate()` already produces a
        // `UnknownStructField { suggestion }` for typos. The
        // collected variant must preserve this — the LSP previously
        // emitted a plain "Unknown field" message and lost the
        // suggestion, which #2214 fixes.
        let ty = struct_type(
            "Versioning",
            vec![StructField::new("status", AttributeType::String)],
        );
        let v = map_value(vec![("statuus", Value::String("x".to_string()))]);
        let errors = ty.validate_collect(&v);
        assert_eq!(errors.len(), 1);
        match &errors[0].1 {
            TypeError::UnknownStructField {
                suggestion: Some(s),
                ..
            } => assert_eq!(s, "status"),
            other => panic!("expected UnknownStructField with suggestion, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// #2220 — ExpectedEnumVariant structured candidates
// ---------------------------------------------------------------------------

#[test]
fn expected_enum_variant_display_namespaced_matches_legacy_format() {
    // The Display impl must reproduce the pre-#2220 rendered string
    // byte-for-byte so existing CLI / LSP messages stay stable.
    let v = ExpectedEnumVariant {
        provider: Some("awscc".to_string()),
        segments: vec!["sso".to_string(), "Assignment".to_string()],
        type_name: "TargetType".to_string(),
        value: "AWS_ACCOUNT".to_string(),
        is_alias: false,
    };
    assert_eq!(v.to_string(), "awscc.sso.Assignment.TargetType.AWS_ACCOUNT");
}

#[test]
fn expected_enum_variant_display_bare_when_no_provider() {
    // Non-namespaced enums (`provider = None`) must render only the
    // bare value — matching how the legacy formatter pushed `v.to_string()`
    // when `namespace` was `None`.
    let v = ExpectedEnumVariant {
        provider: None,
        segments: Vec::new(),
        type_name: "Mode".to_string(),
        value: "fast".to_string(),
        is_alias: false,
    };
    assert_eq!(v.to_string(), "fast");
}

#[test]
fn expected_enum_variant_construction_splits_namespace() {
    // Namespace head becomes `provider`, the rest become `segments`.
    let v = ExpectedEnumVariant::from_namespaced(
        Some("aws.s3.Bucket"),
        "VersioningStatus",
        "Enabled",
        false,
    );
    assert_eq!(v.provider.as_deref(), Some("aws"));
    assert_eq!(v.segments, vec!["s3".to_string(), "Bucket".to_string()]);
    assert_eq!(v.type_name, "VersioningStatus");
    assert_eq!(v.value, "Enabled");
    assert!(!v.is_alias);
}

#[test]
fn expected_includes_to_dsl_aliases_with_alias_flag() {
    // When the schema declares a `to_dsl` mapping, both the canonical
    // value and the alias appear in `expected`. The alias must be
    // marked `is_alias = true` so consumers (LSP code action) can
    // prefer the canonical form.
    fn lower(v: &str) -> String {
        v.to_ascii_lowercase()
    }
    let t = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: Some("aws.s3.Bucket".to_string()),
        to_dsl: Some(lower),
    };
    let err = t.validate(&Value::String("zzz".to_string())).unwrap_err();
    let TypeError::InvalidEnumVariant { expected, .. } = err else {
        panic!("expected InvalidEnumVariant, got {err:?}");
    };
    let canonical: Vec<_> = expected.iter().filter(|e| !e.is_alias).collect();
    let aliases: Vec<_> = expected.iter().filter(|e| e.is_alias).collect();
    assert_eq!(
        canonical
            .iter()
            .map(|e| e.value.as_str())
            .collect::<Vec<_>>(),
        vec!["Enabled", "Suspended"]
    );
    assert_eq!(
        aliases.iter().map(|e| e.value.as_str()).collect::<Vec<_>>(),
        vec!["enabled", "suspended"],
        "to_dsl aliases must be present and tagged is_alias=true"
    );
    // Every entry round-trips through Display in the legacy form.
    assert!(
        canonical
            .iter()
            .all(|e| e.to_string().starts_with("aws.s3.Bucket.VersioningStatus."))
    );
}

#[test]
fn custom_namespaced_string_literal_routes_validator_text_to_extra_message() {
    // For `AttributeType::Custom` namespaced types the validator
    // produces an unstructured message. Pre-#2220 it rode in the
    // `expected: Vec<String>` slot (as a single-element vec). After
    // #2220 the variants slot is structured, so the validator's text
    // moves to `extra_message`. The rendered Display output must stay
    // byte-identical.
    fn validate_mode(v: &Value) -> Result<(), String> {
        match v {
            Value::String(s) if s == "test.r.Mode.fast" => Ok(()),
            Value::String(s) => Err(format!("invalid Mode '{}': expected fast", s)),
            _ => Err("expected String".to_string()),
        }
    }
    let schema = ResourceSchema::new("test.r.mode_holder").attribute(
        AttributeSchema::new(
            "mode",
            AttributeType::Custom {
                semantic_name: Some("Mode".to_string()),
                base: Box::new(AttributeType::String),
                pattern: None,
                length: None,
                validate: validate_mode,
                namespace: Some("test.r".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert("mode".to_string(), Value::String("aaa".to_string()));
    let errs = schema
        .validate_with_origins(&attrs, &|n| n == "mode")
        .unwrap_err();
    let reshaped = errs
        .into_iter()
        .find(|e| matches!(e, TypeError::StringLiteralExpectedEnum { .. }))
        .expect("Custom-namespaced literal must reshape into StringLiteralExpectedEnum");
    let TypeError::StringLiteralExpectedEnum {
        expected,
        extra_message,
        ..
    } = &reshaped
    else {
        unreachable!();
    };
    assert!(
        expected.is_empty(),
        "Custom-namespaced reshape leaves structured variants empty"
    );
    let msg = extra_message
        .as_deref()
        .expect("validator text must be carried on extra_message");
    assert!(
        msg.contains("invalid Mode") && msg.contains("expected fast"),
        "extra_message must carry validator text, got: {msg}"
    );
    let rendered = reshaped.to_string();
    assert!(
        rendered.contains(msg),
        "Display must surface extra_message text in tail, got: {rendered}"
    );
    assert!(
        rendered.contains("expects an enum identifier"),
        "Display must keep the shape-mismatch wording, got: {rendered}"
    );
}

#[test]
fn expected_enum_variant_serde_round_trip() {
    // The LSP carries `Vec<ExpectedEnumVariant>` through
    // `Diagnostic.data` (an opaque JSON value) and reads it back on
    // `textDocument/codeAction` requests. Lock the serde shape so a
    // refactor that renames a field cannot silently break the LSP
    // payload contract. See #2309.
    let original = ExpectedEnumVariant {
        provider: Some("awscc".to_string()),
        segments: vec!["sso".to_string(), "Assignment".to_string()],
        type_name: "TargetType".to_string(),
        value: "AWS_ACCOUNT".to_string(),
        is_alias: false,
    };
    let json = serde_json::to_value(&original).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "provider": "awscc",
            "segments": ["sso", "Assignment"],
            "type_name": "TargetType",
            "value": "AWS_ACCOUNT",
            "is_alias": false,
        }),
        "JSON shape changed — LSP payload contract is at risk"
    );
    let round: ExpectedEnumVariant = serde_json::from_value(json).expect("deserialize");
    assert_eq!(round, original);
}

// ---------------------------------------------------------------------------
// #2219 — Union member-match scoring
// ---------------------------------------------------------------------------
// On a Union failure, surface the closest-matching member's error rather
// than a generic TypeMismatch. "Closest" is measured by structural
// distance: same outer constructor (Map↔Struct, List↔List, String↔
// StringEnum/Custom) wins over an unrelated member. Tie-broken by
// declaration order, so the existing Map/Struct case continues to pick
// the Struct member's error first.

#[test]
fn union_string_vs_string_enum_picks_enum_error_for_string_input() {
    // Acceptance #2 from #2219: `Int | StringEnum` with a string input
    // that doesn't match any enum variant must surface the
    // `InvalidEnumVariant` error (so the user sees `expected one of:
    // fast, slow`), not a generic `TypeMismatch`.
    let union_type = AttributeType::Union(vec![
        AttributeType::Int,
        AttributeType::StringEnum {
            name: "Mode".to_string(),
            values: vec!["fast".to_string(), "slow".to_string()],
            namespace: None,
            to_dsl: None,
        },
    ]);
    let err = union_type
        .validate(&Value::String("zzz".to_string()))
        .unwrap_err();
    match err {
        TypeError::InvalidEnumVariant { ref expected, .. } => {
            let rendered: Vec<String> = expected.iter().map(ToString::to_string).collect();
            assert!(
                rendered.iter().any(|s| s == "fast"),
                "expected `fast` in candidate list, got: {rendered:?}"
            );
        }
        other => panic!("expected InvalidEnumVariant from the StringEnum member, got: {other:?}"),
    }
}

#[test]
fn union_list_vs_list_struct_picks_inner_struct_error() {
    // List<String> | List<Struct{...}>: input is a List of Maps where
    // one Map has an unknown field. The List<Struct> member's nested
    // `UnknownStructField { field: "typo", ... }` error should surface
    // — the user has to know that "typo" isn't a valid field on Item.
    let union_type = AttributeType::Union(vec![
        AttributeType::list(AttributeType::String),
        AttributeType::list(AttributeType::Struct {
            name: "Item".to_string(),
            fields: vec![StructField::new("name", AttributeType::String)],
        }),
    ]);
    let mut bad = IndexMap::new();
    bad.insert("name".to_string(), Value::String("x".to_string()));
    bad.insert("typo".to_string(), Value::String("y".to_string()));
    let value = Value::List(vec![Value::Map(bad)]);
    let err = union_type.validate(&value).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("typo"),
        "expected the inner Unknown-field error to mention `typo`, got: {msg}"
    );
}

#[test]
fn union_string_vs_custom_picks_custom_error_for_string_input() {
    // String | Custom { base = String, validate = predicate }:
    // a string input that fails the Custom predicate should surface
    // the Custom validator's message, not a generic TypeMismatch.
    fn must_be_arn(v: &Value) -> Result<(), String> {
        match v {
            Value::String(s) if s.starts_with("arn:") => Ok(()),
            _ => Err("must start with 'arn:'".to_string()),
        }
    }
    let union_type = AttributeType::Union(vec![
        AttributeType::Int,
        AttributeType::Custom {
            semantic_name: Some("Arn".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: must_be_arn,
            namespace: None,
            to_dsl: None,
        },
    ]);
    let err = union_type
        .validate(&Value::String("not-an-arn".to_string()))
        .unwrap_err();
    match err {
        TypeError::ValidationFailed { ref message } => {
            assert!(
                message.contains("must start with 'arn:'"),
                "expected the Custom validator's actual message, got: {message}"
            );
        }
        other => {
            panic!("expected ValidationFailed from the Custom member's predicate, got: {other:?}")
        }
    }
}

#[test]
fn union_falls_through_to_type_mismatch_when_no_member_matches_shape() {
    // Int | Bool: input is a Map. Neither member shares a constructor
    // with Map. The result is a generic TypeMismatch — there's no
    // "closer" candidate to surface.
    let union_type = AttributeType::Union(vec![AttributeType::Int, AttributeType::Bool]);
    let mut map = IndexMap::new();
    map.insert("k".to_string(), Value::String("v".to_string()));
    let err = union_type.validate(&Value::Map(map)).unwrap_err();
    match err {
        TypeError::TypeMismatch { .. } => {}
        other => panic!("expected TypeMismatch, got: {other:?}"),
    }
}

#[test]
fn union_custom_with_int_base_picks_custom_error_for_int_input() {
    // Custom { base = Int, validate = predicate }: a `Custom` Union
    // member whose declared `base` is `Int` (not `String`) must still
    // be reachable. With an `Int | positive_int()` Union and a
    // negative integer input, the Custom validator's
    // `ValidationFailed` message must surface — not the generic
    // `TypeMismatch` from the bare `Int` member's success path.
    fn must_be_positive(v: &Value) -> Result<(), String> {
        match v {
            Value::Int(n) if *n > 0 => Ok(()),
            _ => Err("must be positive".to_string()),
        }
    }
    // Flip the order so the bare `Int` arm doesn't accept the value
    // first — both members run validate(). Bare `Int::validate`
    // accepts any `Value::Int`, so we have to keep it second; bind
    // through a Custom on top so the actual reachable failure path
    // is the `Custom` one.
    let union_type = AttributeType::Union(vec![
        AttributeType::Custom {
            semantic_name: Some("PositiveInt".to_string()),
            base: Box::new(AttributeType::Int),
            pattern: None,
            length: None,
            validate: must_be_positive,
            namespace: None,
            to_dsl: None,
        },
        AttributeType::Bool,
    ]);
    let err = union_type.validate(&Value::Int(-5)).unwrap_err();
    match err {
        TypeError::ValidationFailed { ref message } => {
            assert!(
                message.contains("must be positive"),
                "expected the Custom validator's actual message, got: {message}"
            );
        }
        other => {
            panic!("expected ValidationFailed from the Custom-with-Int-base member, got: {other:?}")
        }
    }
}

#[test]
fn union_struct_member_still_wins_for_map_input_regression() {
    // Regression for the original heuristic: Map input + Struct member
    // surfaces the Struct's "Unknown field" error. This test mirrors
    // `union_struct_unknown_field_shows_specific_error` but is kept
    // here so a future scoring tweak that drops this case fails
    // immediately.
    let union_type = AttributeType::Union(vec![
        AttributeType::Struct {
            name: "Principal".to_string(),
            fields: vec![StructField::new("service", AttributeType::String)],
        },
        AttributeType::String,
    ]);
    let mut map = IndexMap::new();
    map.insert("service".to_string(), Value::String("x".to_string()));
    map.insert("typo".to_string(), Value::String("y".to_string()));
    let err = union_type.validate(&Value::Map(map)).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("typo") && msg.contains("Principal"),
        "Struct member must still win for Map input, got: {msg}"
    );
}
