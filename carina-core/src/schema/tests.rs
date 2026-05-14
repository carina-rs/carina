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
fn resource_schema_kind_default_managed() {
    let schema = ResourceSchema::new("test.resource");
    assert_eq!(schema.kind, SchemaKind::Managed);
    assert!(!schema.is_data_source());
}

#[test]
fn resource_schema_as_data_source_sets_kind() {
    let schema = ResourceSchema::new("test.resource").as_data_source();
    assert_eq!(schema.kind, SchemaKind::DataSource);
    assert!(schema.is_data_source());
}

#[test]
fn validate_string_type() {
    let t = AttributeType::String;
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("hello".to_string())))
            .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    );
}

#[test]
fn validate_string_enum_type() {
    // Phase 4 of carina#2986: enum attributes accept only
    // `ConcreteValue::EnumIdentifier`. Constructed-by-hand strings are
    // rejected as `StringLiteralExpectedEnum` — see
    // `validate_string_enum_rejects_quoted_string_literal` for that path.
    let t = AttributeType::StringEnum {
        name: "AddressFamily".to_string(),
        values: vec!["IPv4".to_string(), "IPv6".to_string()],
        namespace: Some("awscc.ec2.ipam_pool".to_string()),
        dsl_aliases: vec![],
    };
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "awscc.ec2.ipam_pool.AddressFamily.IPv4".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "IPv6".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "ipv4".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "IPv5".to_string()
        )))
        .is_err()
    );
}

#[test]
fn validate_string_enum_rejects_quoted_string_literal() {
    // Phase 4 of carina#2986: a `String`-shaped value on a `StringEnum`
    // attribute means the user wrote `attr = "value"`. The validator
    // emits `StringLiteralExpectedEnum` with the full expected variant
    // list so the LSP code action can offer "drop quotes / use
    // identifier form" without re-deriving candidates.
    let t = AttributeType::StringEnum {
        name: "AddressFamily".to_string(),
        values: vec!["IPv4".to_string(), "IPv6".to_string()],
        namespace: Some("awscc.ec2.ipam_pool".to_string()),
        dsl_aliases: vec![],
    };
    let err = t
        .validate(&Value::Concrete(ConcreteValue::String("IPv4".to_string())))
        .unwrap_err();
    assert!(
        matches!(err, TypeError::StringLiteralExpectedEnum { ref user_typed, .. } if user_typed == "IPv4"),
        "expected StringLiteralExpectedEnum, got: {err:?}"
    );
}

#[test]
fn string_enum_type_name_uses_declared_name() {
    let t = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: Some("aws.s3.Bucket".to_string()),
        dsl_aliases: vec![],
    };
    assert_eq!(t.type_name(), "VersioningStatus");
}

#[test]
fn validate_string_enum_accepts_dsl_alias() {
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
        dsl_aliases: vec![("-1".to_string(), "all".to_string())],
    };
    // Canonical "-1" is rewritten to DSL "all" — the DSL surface must
    // not accept the API form when an alias is registered. Users
    // write `all`. Updated for carina#2980 / carina#2986.
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "-1".to_string()
        )))
        .is_err()
    );
    // DSL alias "all" should be accepted
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "awscc.ec2.SecurityGroup.IpProtocol.all".to_string()
        )))
        .is_ok()
    );
    // Other canonical values without an alias rewrite (e.g. `tcp`,
    // which is already snake_case) keep working — the API spelling
    // *is* the DSL spelling for them.
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "tcp".to_string()
        )))
        .is_ok()
    );
    // Invalid values should still be rejected
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "invalid".to_string()
        )))
        .is_err()
    );
}

#[test]
fn validate_string_enum_all_without_dsl_aliases_requires_explicit_variant() {
    // Without "all" as a direct variant or in `dsl_aliases`, the bare
    // identifier `all` is not accepted. Issue #1428.
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
        dsl_aliases: vec![],
    };
    // Without "all" in values and no dsl_aliases entry mapping to "all", it is rejected
    assert!(
        without_all
            .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
                "all".to_string()
            )))
            .is_err()
    );

    // With "all" added to values, it is accepted even without dsl_aliases
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
        dsl_aliases: vec![],
    };
    assert!(
        with_all
            .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
                "all".to_string()
            )))
            .is_ok()
    );
}

#[test]
fn validate_string_enum_accepts_values_with_dots() {
    // Values like "ipsec.1" contain dots that should not be treated as
    // namespace separators (issue #611)
    let t = AttributeType::StringEnum {
        name: "Type".to_string(),
        values: vec!["ipsec.1".to_string()],
        namespace: Some("awscc.ec2.vpn_gateway".to_string()),
        dsl_aliases: vec![],
    };
    // Bare identifier with dot should match directly (carried as
    // `EnumIdentifier` under strict mode — the test name still says
    // "Quoted string" for historical reasons but values are written
    // unquoted in real DSL).
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "ipsec.1".to_string()
        )))
        .is_ok()
    );
    // Fully qualified form should also be accepted
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
        )))
        .is_ok()
    );
    // Invalid value should still be rejected
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "ipsec.2".to_string()
        )))
        .is_err()
    );
}

#[test]
fn invalid_enum_error_preserves_user_typed_string_literal() {
    // Regression for #2077, updated for carina#2986: a quoted string
    // literal `target_type = "aaa"` now reaches the validator as
    // `ConcreteValue::String("aaa")` and is rejected as
    // `StringLiteralExpectedEnum`. The user-typed value must surface in
    // the error verbatim (echoed inside double quotes per the
    // `format_string_literal_expected_enum` shape) without leaking the
    // synthesized namespaced form.
    let t = AttributeType::StringEnum {
        name: "TargetType".to_string(),
        values: vec!["AWS_ACCOUNT".to_string()],
        namespace: Some("awscc.sso.Assignment".to_string()),
        dsl_aliases: vec![],
    };
    let err = t
        .validate(&Value::Concrete(ConcreteValue::String("aaa".to_string())))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("\"aaa\""),
        "error should echo the user's typed value, got: {msg}"
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
        dsl_aliases: vec![],
    };
    let err = t
        .validate(&Value::Concrete(ConcreteValue::String("aaa".to_string())))
        .unwrap_err();
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
    // Regression for #2098, updated for carina#2986. The
    // `with_attribute` plumbing routes the attribute name onto both
    // `InvalidEnumVariant` (for genuine wrong-variant inputs) and
    // `StringLiteralExpectedEnum` (the strict-mode quoted-string
    // rejection). Pin both behaviors:
    let t = AttributeType::StringEnum {
        name: "TargetType".to_string(),
        values: vec!["AWS_ACCOUNT".to_string()],
        namespace: Some("awscc.sso.Assignment".to_string()),
        dsl_aliases: vec![],
    };
    // String-literal path (strict-mode rejection): rendered as
    // `'target_id' (TargetType) expects an enum identifier, got a
    // string literal "aaa"...`.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::String("aaa".to_string())))
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
        msg.contains("\"aaa\""),
        "error should still echo the user value, got: {msg}"
    );

    // Identifier path (`InvalidEnumVariant`): the message keeps the
    // single-quoted form for the bare value.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "aaa".to_string(),
        )))
        .unwrap_err()
        .with_attribute("target_id");
    let msg = err.to_string();
    assert!(
        msg.contains("'target_id'"),
        "identifier-path error should quote attribute name, got: {msg}"
    );
    assert!(
        msg.contains("'aaa'"),
        "identifier-path error should single-quote the bare value, got: {msg}"
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
                dsl_aliases: vec![],
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".to_string())),
    );
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
    // string literals. Under carina#2986 strict mode the `Value`
    // variant alone already encodes that distinction
    // (`ConcreteValue::String` = quoted literal,
    // `ConcreteValue::EnumIdentifier` = bare/namespaced identifier),
    // so the diagnostic shape follows from the value's variant
    // independently of the origin tag.
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::new(
            "target_type",
            AttributeType::StringEnum {
                name: "TargetType".to_string(),
                values: vec!["AWS_ACCOUNT".to_string()],
                namespace: Some("awscc.sso.Assignment".to_string()),
                dsl_aliases: vec![],
            },
        )
        .required(),
    );

    // Quoted literal → reshaped diagnostic, regardless of origin tag.
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".to_string())),
    );
    let errs = schema
        .validate_with_origins(&attrs, &|name| name == "target_type")
        .unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, TypeError::StringLiteralExpectedEnum { .. })),
        "expected StringLiteralExpectedEnum variant, got: {errs:?}"
    );

    // Bare-identifier wrong-variant → classic InvalidEnumVariant.
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::Concrete(ConcreteValue::EnumIdentifier("aaa".to_string())),
    );
    let errs = schema
        .validate_with_origins(&attrs, &|_| false)
        .unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, TypeError::InvalidEnumVariant { .. })),
        "expected InvalidEnumVariant variant for bare identifier, got: {errs:?}"
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
                dsl_aliases: vec![],
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::Concrete(ConcreteValue::EnumIdentifier("AWS_ACCOUNT".to_string())),
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
            Value::Concrete(ConcreteValue::String(s)) if s == "test.r.Mode.fast" => Ok(()),
            Value::Concrete(ConcreteValue::String(s)) => {
                Err(format!("invalid Mode '{}': expected fast", s))
            }
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
                validate: legacy_validator(validate_mode),
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
    attrs.insert(
        "mode".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".to_string())),
    );
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
        dsl_aliases: vec![],
    };
    let err = t
        .validate(&Value::Concrete(ConcreteValue::String("zzz".to_string())))
        .unwrap_err();
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
    // path produces the same `Value::Concrete(ConcreteValue::String(...))`), the error echoes the
    // full form back — still the "user-typed" form because that's what
    // was in the Value. This verifies the fix doesn't regress that case.
    let t = AttributeType::StringEnum {
        name: "TargetType".to_string(),
        values: vec!["AWS_ACCOUNT".to_string()],
        namespace: Some("awscc.sso.Assignment".to_string()),
        dsl_aliases: vec![],
    };
    let input = "awscc.sso.Assignment.TargetType.NOT_REAL".to_string();
    let err = t
        .validate(&Value::Concrete(ConcreteValue::String(input.clone())))
        .unwrap_err();
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
        dsl_aliases: vec![],
    };
    // Double-namespace must be rejected
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "awscc.ec2.Vpc.InstanceTenancy.awscc.ec2.Vpc.InstanceTenancy.default".to_string()
        )))
        .is_err()
    );
}

#[test]
fn validate_float_type() {
    let t = AttributeType::Float;
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Float(2.5)))
            .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Float(-0.5)))
            .is_ok()
    );
    assert!(t.validate(&Value::Concrete(ConcreteValue::Int(42))).is_ok()); // integers are valid numbers
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("3.14".to_string())))
            .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Bool(true)))
            .is_err()
    );
}

#[test]
fn validate_float_rejects_non_finite() {
    let t = AttributeType::Float;
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Float(f64::NAN)))
            .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Float(f64::INFINITY)))
            .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Float(f64::NEG_INFINITY)))
            .is_err()
    );
}

#[test]
fn validate_int_rejects_float() {
    let t = AttributeType::Int;
    assert!(t.validate(&Value::Concrete(ConcreteValue::Int(42))).is_ok());
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Float(2.5)))
            .is_err()
    ); // strict integer typing
}

#[test]
fn validate_positive_int() {
    let t = types::positive_int();
    assert!(t.validate(&Value::Concrete(ConcreteValue::Int(1))).is_ok());
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(100)))
            .is_ok()
    );
    assert!(t.validate(&Value::Concrete(ConcreteValue::Int(0))).is_err());
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(-1)))
            .is_err()
    );
}

#[test]
fn validate_resource_schema() {
    let schema = ResourceSchema::new("resource")
        .attribute(AttributeSchema::new("name", AttributeType::String).required())
        .attribute(AttributeSchema::new("count", types::positive_int()))
        .attribute(AttributeSchema::new("enabled", AttributeType::Bool));

    let mut attrs = HashMap::new();
    attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("my-resource".to_string())),
    );
    attrs.insert("count".to_string(), Value::Concrete(ConcreteValue::Int(5)));
    attrs.insert(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );

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
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/16".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "192.168.1.0/24".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "0.0.0.0/0".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "255.255.255.255/32".to_string()
        )))
        .is_ok()
    );

    // Invalid CIDRs
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0".to_string()
        )))
        .is_err()
    ); // no prefix
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/33".to_string()
        )))
        .is_err()
    ); // prefix too large
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.256/16".to_string()
        )))
        .is_err()
    ); // octet > 255
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0/16".to_string()
        )))
        .is_err()
    ); // only 3 octets
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "invalid".to_string()
        )))
        .is_err()
    ); // not a CIDR
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    ); // wrong type
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
    map.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".to_string())),
    );
    map.insert(
        "from_port".to_string(),
        Value::Concrete(ConcreteValue::Int(80)),
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Map(map)))
            .is_ok()
    );

    // Invalid: missing required field
    let empty_map = IndexMap::new();
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Map(empty_map)))
            .is_err()
    );

    // Invalid: wrong type for field
    let mut bad_map = IndexMap::new();
    bad_map.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".to_string())),
    );
    bad_map.insert(
        "from_port".to_string(),
        Value::Concrete(ConcreteValue::String("not_a_number".to_string())),
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Map(bad_map)))
            .is_err()
    );

    // Invalid: not a Map
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "not a struct".to_string()
        )))
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
    map.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".to_string())),
    );
    map.insert(
        "unknown_field".to_string(),
        Value::Concrete(ConcreteValue::String("value".to_string())),
    );
    let result = t.validate(&Value::Concrete(ConcreteValue::Map(map)));
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
    map.insert(
        "ip_protcol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".to_string())),
    );
    let result = t.validate(&Value::Concrete(ConcreteValue::Map(map)));
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
    map2.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".to_string())),
    );
    map2.insert(
        "cidr_iip".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/8".to_string())),
    );
    let result2 = t.validate(&Value::Concrete(ConcreteValue::Map(map2)));
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
    map.insert(
        "vpc_idd".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
    );
    let err = t
        .validate(&Value::Concrete(ConcreteValue::Map(map)))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Unknown field 'vpc_idd' in SecurityGroupIngress, did you mean 'vpc_id'?"
    );

    // Without suggestion (completely different name)
    let mut map2 = IndexMap::new();
    map2.insert(
        "completely_different".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let err2 = t
        .validate(&Value::Concrete(ConcreteValue::Map(map2)))
        .unwrap_err();
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
    item.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".to_string())),
    );
    let list = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(item),
    )]));
    assert!(list_type.validate(&list).is_ok());

    // Invalid item in list
    let bad_list = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(IndexMap::new()),
    )]));
    assert!(list_type.validate(&bad_list).is_err());
}

#[test]
fn struct_rejects_block_syntax_single_element() {
    // Block syntax produces Value::Concrete(ConcreteValue::List([Value::Map(...)])) which should be rejected
    // for bare Struct attributes
    let struct_type = AttributeType::Struct {
        name: "VersioningConfiguration".to_string(),
        fields: vec![StructField::new("status", AttributeType::String).required()],
    };

    let mut map = IndexMap::new();
    map.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("Enabled".to_string())),
    );
    let single_list = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(map),
    )]));
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
    map1.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("Enabled".to_string())),
    );
    let mut map2 = IndexMap::new();
    map2.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("Suspended".to_string())),
    );
    let multi_list = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(map1)),
        Value::Concrete(ConcreteValue::Map(map2)),
    ]));
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
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/16".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "0.0.0.0/0".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "255.255.255.255/32".to_string()
        )))
        .is_ok()
    );

    // Invalid IPv4 CIDRs
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/33".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    );
}

#[test]
fn validate_ipv6_cidr_type() {
    let t = types::ipv6_cidr();

    // Valid IPv6 CIDRs
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("::/0".to_string())))
            .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:db8::/32".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "fe80::/10".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "::1/128".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:0db8:85a3:0000:0000:8a2e:0370:7334/64".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "ff00::/8".to_string()
        )))
        .is_ok()
    );

    // Invalid IPv6 CIDRs
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:db8::/129".to_string()
        )))
        .is_err()
    ); // prefix > 128
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:db8::".to_string()
        )))
        .is_err()
    ); // missing prefix
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:gggg::/32".to_string()
        )))
        .is_err()
    ); // invalid hex
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:db8::1::2/64".to_string()
        )))
        .is_err()
    ); // double ::
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/16".to_string()
        )))
        .is_err()
    ); // IPv4, not IPv6
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    ); // wrong type
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
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/16".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "0.0.0.0/0".to_string()
        )))
        .is_ok()
    );

    // Valid IPv6 CIDRs
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:db8::/32".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("::/0".to_string())))
            .is_ok()
    );

    // Invalid
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "not-a-cidr".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0".to_string()
        )))
        .is_err()
    ); // no prefix
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    );
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
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.1.5".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "192.168.0.1".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "0.0.0.0".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "255.255.255.255".to_string()
        )))
        .is_ok()
    );

    // Invalid IPv4 addresses
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/16".to_string()
        )))
        .is_err()
    ); // CIDR, not address
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "256.0.0.1".to_string()
        )))
        .is_err()
    ); // octet > 255
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "10.0.1".to_string()
        )))
        .is_err()
    ); // only 3 octets
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "not-an-ip".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    ); // wrong type
}

#[test]
fn validate_ipv6_address_type() {
    let t = types::ipv6_address();

    // Valid IPv6 addresses
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("::1".to_string())))
            .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:db8::1".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "fe80::1".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:0db8:85a3:0000:0000:8a2e:0370:7334".to_string()
        )))
        .is_ok()
    );

    // Invalid IPv6 addresses
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "2001:db8::/32".to_string()
        )))
        .is_err()
    ); // CIDR, not address
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "not-an-ip".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("".to_string())))
            .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    ); // wrong type
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
    attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("test".to_string())),
    );
    assert!(schema.validate(&attrs).is_ok());

    // Invalid: forbidden attribute present
    let mut bad_attrs = HashMap::new();
    bad_attrs.insert(
        "forbidden".to_string(),
        Value::Concrete(ConcreteValue::String("bad".to_string())),
    );
    let result = schema.validate(&bad_attrs);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().len(), 1);
}

#[test]
fn validate_exclusive_required_helper() {
    use validators::validate_exclusive_required;

    // Valid: exactly one field present
    let mut attrs = HashMap::new();
    attrs.insert(
        "option_a".to_string(),
        Value::Concrete(ConcreteValue::String("value".to_string())),
    );
    assert!(validate_exclusive_required(&attrs, &["option_a", "option_b"]).is_ok());

    let mut attrs2 = HashMap::new();
    attrs2.insert(
        "option_b".to_string(),
        Value::Concrete(ConcreteValue::String("value".to_string())),
    );
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
    both.insert(
        "option_a".to_string(),
        Value::Concrete(ConcreteValue::String("a".to_string())),
    );
    both.insert(
        "option_b".to_string(),
        Value::Concrete(ConcreteValue::String("b".to_string())),
    );
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
    attrs1.insert(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
    );
    attrs1.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/24".to_string())),
    );
    assert!(schema.validate(&attrs1).is_ok());

    // Valid: has ipv4_ipam_pool_id only
    let mut attrs2 = HashMap::new();
    attrs2.insert(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
    );
    attrs2.insert(
        "ipv4_ipam_pool_id".to_string(),
        Value::Concrete(ConcreteValue::String("ipam-pool-123".to_string())),
    );
    assert!(schema.validate(&attrs2).is_ok());

    // Invalid: has neither
    let mut attrs3 = HashMap::new();
    attrs3.insert(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
    );
    let result = schema.validate(&attrs3);
    assert!(result.is_err());

    // Invalid: has both
    let mut attrs4 = HashMap::new();
    attrs4.insert(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
    );
    attrs4.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/24".to_string())),
    );
    attrs4.insert(
        "ipv4_ipam_pool_id".to_string(),
        Value::Concrete(ConcreteValue::String("ipam-pool-123".to_string())),
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
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
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
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    both.insert(
        "ipv4_ipam_pool_id".to_string(),
        Value::Concrete(ConcreteValue::String("pool-1".to_string())),
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
    ok.insert(
        "a".to_string(),
        Value::Concrete(ConcreteValue::String("1".to_string())),
    );
    ok.insert(
        "x".to_string(),
        Value::Concrete(ConcreteValue::String("1".to_string())),
    );
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
        validate: legacy_validator(|value| {
            if let Value::Concrete(ConcreteValue::String(s)) = value {
                if s.starts_with("a-") {
                    Ok(())
                } else {
                    Err(format!("Expected 'a-' prefix, got '{}'", s))
                }
            } else {
                Err("Expected string".to_string())
            }
        }),
        namespace: None,
        to_dsl: None,
    };
    let type_b = AttributeType::Custom {
        semantic_name: Some("TypeB".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: legacy_validator(|value| {
            if let Value::Concrete(ConcreteValue::String(s)) = value {
                if s.starts_with("b-") {
                    Ok(())
                } else {
                    Err(format!("Expected 'b-' prefix, got '{}'", s))
                }
            } else {
                Err("Expected string".to_string())
            }
        }),
        namespace: None,
        to_dsl: None,
    };

    let union_type = AttributeType::Union(vec![type_a, type_b]);

    // Valid: matches first member
    assert!(
        union_type
            .validate(&Value::Concrete(ConcreteValue::String(
                "a-12345678".to_string()
            )))
            .is_ok()
    );
    // Valid: matches second member
    assert!(
        union_type
            .validate(&Value::Concrete(ConcreteValue::String(
                "b-12345678".to_string()
            )))
            .is_ok()
    );
    // Invalid: matches neither
    assert!(
        union_type
            .validate(&Value::Concrete(ConcreteValue::String(
                "c-12345678".to_string()
            )))
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
        Value::Concrete(ConcreteValue::String("arn:...".to_string())),
    );
    map.insert(
        "aaa".to_string(),
        Value::Concrete(ConcreteValue::String("bbb".to_string())),
    );

    let err = principal_type
        .validate(&Value::Concrete(ConcreteValue::Map(map)))
        .unwrap_err();
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
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
    };
    let type_b = AttributeType::Custom {
        semantic_name: Some("TypeB".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: noop_validator(),
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
        validate: noop_validator(),
        namespace: None,
        to_dsl: None,
    };
    let type_b = AttributeType::Custom {
        semantic_name: Some("TypeB".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: noop_validator(),
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
        // Block syntax produces Value::Concrete(ConcreteValue::List)
        r.set_attr(
            "operating_region".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map({
                    let mut m = IndexMap::new();
                    m.insert(
                        "region_name".to_string(),
                        Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
                    );
                    m
                }),
            )])),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.ipam").attribute(
            AttributeSchema::new("operating_regions", AttributeType::String)
                .with_block_name("operating_region"),
        ),
    );

    resolve_block_names(&mut resources, &schemas).unwrap();

    assert!(resources[0].attributes.contains_key("operating_regions"));
    assert!(!resources[0].attributes.contains_key("operating_region"));
}

#[test]
fn resolve_block_names_noop_when_no_match() {
    let mut resources = vec![{
        let mut r = Resource::new("ec2.ipam", "my-ipam");
        r.set_attr(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.ipam")
            .attribute(AttributeSchema::new("name", AttributeType::String)),
    );

    resolve_block_names(&mut resources, &schemas).unwrap();

    assert!(resources[0].attributes.contains_key("name"));
}

#[test]
fn resolve_block_names_errors_on_mixed_syntax() {
    let mut resources = vec![{
        let mut r = Resource::new("ec2.ipam", "my-ipam");
        // Block syntax produces Value::Concrete(ConcreteValue::List)
        r.set_attr(
            "operating_region".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map({
                    let mut m = IndexMap::new();
                    m.insert(
                        "region_name".to_string(),
                        Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
                    );
                    m
                }),
            )])),
        );
        // User also explicitly set the canonical name
        r.set_attr(
            "operating_regions".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map({
                    let mut m = IndexMap::new();
                    m.insert(
                        "region_name".to_string(),
                        Value::Concrete(ConcreteValue::String("us-west-2".to_string())),
                    );
                    m
                }),
            )])),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("ec2.ipam").attribute(
            AttributeSchema::new("operating_regions", AttributeType::String)
                .with_block_name("operating_region"),
        ),
    );

    let result = resolve_block_names(&mut resources, &schemas);
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
            Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
        );
        r
    }];

    let schemas = SchemaRegistry::new();

    // Should not error for unknown resource types
    resolve_block_names(&mut resources, &schemas).unwrap();

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
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "storage_class".to_string(),
                    Value::Concrete(ConcreteValue::String("GLACIER".to_string())),
                );
                m
            }),
        )])),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr(
            "lifecycle_configuration".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
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

    resolve_block_names(&mut resources, &schemas).unwrap();

    // The nested "transition" key should be renamed to "transitions"
    let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
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
    // `transition = { ... }` (Value::Concrete(ConcreteValue::Map)) should NOT be renamed to `transitions`.
    // Only block syntax `transition { ... }` (Value::Concrete(ConcreteValue::List)) should be renamed.
    let mut inner_map = IndexMap::new();
    // This is an attribute assignment: transition = { storage_class = "GLACIER" }
    // Parser produces Value::Concrete(ConcreteValue::Map) for attribute assignments
    inner_map.insert(
        "transition".to_string(),
        Value::Concrete(ConcreteValue::Map({
            let mut m = IndexMap::new();
            m.insert(
                "storage_class".to_string(),
                Value::Concrete(ConcreteValue::String("GLACIER".to_string())),
            );
            m
        })),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr(
            "lifecycle_configuration".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
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

    resolve_block_names(&mut resources, &schemas).unwrap();

    let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
        _ => panic!("expected Map"),
    };
    // The Value::Concrete(ConcreteValue::Map) should remain as "transition" (not renamed)
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
    // Block syntax produces Value::Concrete(ConcreteValue::List)
    inner_map.insert(
        "transition".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "storage_class".to_string(),
                    Value::Concrete(ConcreteValue::String("GLACIER".to_string())),
                );
                m
            }),
        )])),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr(
            "lifecycle_configuration".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
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

    resolve_block_names(&mut resources, &schemas).unwrap();

    let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
        _ => panic!("expected Map"),
    };
    // Block syntax (Value::Concrete(ConcreteValue::List)) should be renamed to "transitions"
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
        // Block syntax produces Value::Concrete(ConcreteValue::List)
        r.set_attr(
            "ingress".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map({
                    let mut m = IndexMap::new();
                    m.insert(
                        "ip_protocol".to_string(),
                        Value::Concrete(ConcreteValue::String("tcp".to_string())),
                    );
                    m
                }),
            )])),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
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
    resolve_block_names(&mut resources, &schemas).unwrap();

    // Key should remain as "ingress"
    assert!(resources[0].attributes.contains_key("ingress"));
    // Value should be unchanged
    match resources[0].get_attr("ingress") {
        Some(Value::Concrete(ConcreteValue::List(items))) => assert_eq!(items.len(), 1),
        other => panic!("expected List, got {:?}", other),
    }
}

#[test]
fn resolve_block_names_same_block_and_canonical_name_multiple_items() {
    // When block_name == canonical name and the user provides multiple block
    // items (Value::Concrete(ConcreteValue::List) with multiple entries), no conflict should occur.
    // The key already exists (it IS the canonical key), so the `continue`
    // path handles it. This test verifies all items are preserved.
    let mut resources = vec![{
        let mut r = Resource::new("ec2.SecurityGroup", "my-sg");
        r.set_attr(
            "ingress".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Map({
                    let mut m = IndexMap::new();
                    m.insert(
                        "ip_protocol".to_string(),
                        Value::Concrete(ConcreteValue::String("tcp".to_string())),
                    );
                    m
                })),
                Value::Concrete(ConcreteValue::Map({
                    let mut m = IndexMap::new();
                    m.insert(
                        "ip_protocol".to_string(),
                        Value::Concrete(ConcreteValue::String("udp".to_string())),
                    );
                    m
                })),
            ])),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
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
    resolve_block_names(&mut resources, &schemas).unwrap();

    assert!(resources[0].attributes.contains_key("ingress"));
    match resources[0].get_attr("ingress") {
        Some(Value::Concrete(ConcreteValue::List(items))) => assert_eq!(items.len(), 2),
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
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("Name".to_string())),
                );
                m.insert(
                    "value".to_string(),
                    Value::Concrete(ConcreteValue::String("test".to_string())),
                );
                m
            }),
        )])),
    );

    let mut resources = vec![{
        let mut r = Resource::new("test.resource", "my-resource");
        r.set_attr(
            "config".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        r
    }];

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
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
    resolve_block_names(&mut resources, &schemas).unwrap();

    let config = match resources[0].get_attr("config") {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
        _ => panic!("expected Map"),
    };
    // Key should remain as "tag" (no rename needed since block_name == canonical)
    assert!(
        config.contains_key("tag"),
        "expected 'tag' key to remain (block_name == canonical name)"
    );
    match config.get("tag") {
        Some(Value::Concrete(ConcreteValue::List(items))) => assert_eq!(items.len(), 1),
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
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::new())),
    );

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
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::new())),
    );

    assert!(schema.validate(&attrs).is_ok());
}

#[test]
fn validate_unknown_attribute_with_suggestion() {
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::String));

    let mut attrs = HashMap::new();
    attrs.insert(
        "bukcet_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
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
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("rule1".to_string()),
        )])),
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
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    attrs.insert(
        "_binding".to_string(),
        Value::Concrete(ConcreteValue::String("b".to_string())),
    );

    assert!(schema.validate(&attrs).is_ok());
}

fn make_custom(name: &str, base: AttributeType) -> AttributeType {
    AttributeType::Custom {
        semantic_name: Some(name.to_string()),
        base: Box::new(base),
        pattern: None,
        length: None,
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        validate: noop_validator(),
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
        t.validate(&Value::Concrete(ConcreteValue::String(
            "user@example.com".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "user.name+tag@sub.example.co.jp".to_string()
        )))
        .is_ok()
    );

    // Invalid emails
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "no-at-sign.com".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "noTLD@host".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String(
            "@example.com".to_string()
        )))
        .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("user@".to_string())))
            .is_err()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::String("".to_string())))
            .is_err()
    );

    // Wrong type
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    );
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
        Value::Concrete(ConcreteValue::Map(map))
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
            (
                "status",
                Value::Concrete(ConcreteValue::String("Enabled".to_string())),
            ),
            ("mfa_delete", Value::Concrete(ConcreteValue::Bool(false))),
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
            (
                "statuus",
                Value::Concrete(ConcreteValue::String("Enabled".to_string())),
            ),
            ("mfa", Value::Concrete(ConcreteValue::Bool(false))),
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
            map_value(vec![(
                "count",
                Value::Concrete(ConcreteValue::String("not an int".to_string())),
            )]),
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
        let v = Value::Concrete(ConcreteValue::List(vec![
            map_value(vec![(
                "name",
                Value::Concrete(ConcreteValue::String("ok".to_string())),
            )]),
            map_value(vec![("name", Value::Concrete(ConcreteValue::Int(42)))]),
        ]));
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
        let v = map_value(vec![(
            "transition",
            Value::Concrete(ConcreteValue::String("ok".to_string())),
        )]);
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
        let v = map_value(vec![(
            "statuus",
            Value::Concrete(ConcreteValue::String("x".to_string())),
        )]);
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
    let t = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: Some("aws.s3.Bucket".to_string()),
        dsl_aliases: vec![
            ("Enabled".to_string(), "enabled".to_string()),
            ("Suspended".to_string(), "suspended".to_string()),
        ],
    };
    // Use `EnumIdentifier` so the strict-mode validator reaches the
    // wrong-variant branch (`InvalidEnumVariant`). A `String` here would
    // be rejected earlier as `StringLiteralExpectedEnum` — that path is
    // covered by `validate_string_enum_rejects_quoted_string_literal`.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "zzz".to_string(),
        )))
        .unwrap_err();
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
            Value::Concrete(ConcreteValue::String(s)) if s == "test.r.Mode.fast" => Ok(()),
            Value::Concrete(ConcreteValue::String(s)) => {
                Err(format!("invalid Mode '{}': expected fast", s))
            }
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
                validate: legacy_validator(validate_mode),
                namespace: Some("test.r".to_string()),
                to_dsl: None,
            },
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "mode".to_string(),
        Value::Concrete(ConcreteValue::String("aaa".to_string())),
    );
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
    // Acceptance #2 from #2219: `Int | StringEnum` with an identifier
    // input that doesn't match any enum variant must surface the
    // `InvalidEnumVariant` error from the StringEnum member (so the
    // user sees `expected one of: fast, slow`), not a generic
    // `TypeMismatch` from the Int member.
    //
    // Updated for carina#2986 strict mode: the test input is an
    // `EnumIdentifier`, which is the legitimate identifier-shaped path
    // that lands in the enum-variant matcher. A `String` here would
    // short-circuit to `StringLiteralExpectedEnum`, which is a
    // different concern covered separately.
    let union_type = AttributeType::Union(vec![
        AttributeType::Int,
        AttributeType::StringEnum {
            name: "Mode".to_string(),
            values: vec!["fast".to_string(), "slow".to_string()],
            namespace: None,
            dsl_aliases: vec![],
        },
    ]);
    let err = union_type
        .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "zzz".to_string(),
        )))
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
    bad.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    bad.insert(
        "typo".to_string(),
        Value::Concrete(ConcreteValue::String("y".to_string())),
    );
    let value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(bad),
    )]));
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
            Value::Concrete(ConcreteValue::String(s)) if s.starts_with("arn:") => Ok(()),
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
            validate: legacy_validator(must_be_arn),
            namespace: None,
            to_dsl: None,
        },
    ]);
    let err = union_type
        .validate(&Value::Concrete(ConcreteValue::String(
            "not-an-arn".to_string(),
        )))
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
    map.insert(
        "k".to_string(),
        Value::Concrete(ConcreteValue::String("v".to_string())),
    );
    let err = union_type
        .validate(&Value::Concrete(ConcreteValue::Map(map)))
        .unwrap_err();
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
            Value::Concrete(ConcreteValue::Int(n)) if *n > 0 => Ok(()),
            _ => Err("must be positive".to_string()),
        }
    }
    // Flip the order so the bare `Int` arm doesn't accept the value
    // first — both members run validate(). Bare `Int::validate`
    // accepts any `Value::Concrete(ConcreteValue::Int)`, so we have to keep it second; bind
    // through a Custom on top so the actual reachable failure path
    // is the `Custom` one.
    let union_type = AttributeType::Union(vec![
        AttributeType::Custom {
            semantic_name: Some("PositiveInt".to_string()),
            base: Box::new(AttributeType::Int),
            pattern: None,
            length: None,
            validate: legacy_validator(must_be_positive),
            namespace: None,
            to_dsl: None,
        },
        AttributeType::Bool,
    ]);
    let err = union_type
        .validate(&Value::Concrete(ConcreteValue::Int(-5)))
        .unwrap_err();
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
    map.insert(
        "service".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    map.insert(
        "typo".to_string(),
        Value::Concrete(ConcreteValue::String("y".to_string())),
    );
    let err = union_type
        .validate(&Value::Concrete(ConcreteValue::Map(map)))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("typo") && msg.contains("Principal"),
        "Struct member must still win for Map input, got: {msg}"
    );
}

#[test]
fn custom_validator_can_capture_external_state() {
    // The validator closes over `allowed_region`, captured from the
    // surrounding scope. A `fn` pointer cannot do this; only a
    // closure-capable validator can. This is the core acceptance for
    // #2217 (closure-capable Custom validator).
    let allowed_region = "ap-northeast-1".to_string();
    let attr = AttributeType::Custom {
        semantic_name: Some("Region".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: validator(move |v| match v {
            Value::Concrete(ConcreteValue::String(s)) if s == &allowed_region => Ok(()),
            Value::Concrete(ConcreteValue::String(s)) => Err(TypeError::ValidationFailed {
                message: format!("expected region {}, got {}", allowed_region, s),
            }),
            other => Err(TypeError::TypeMismatch {
                expected: "String".to_string(),
                got: other.type_name(),
            }),
        }),
        namespace: None,
        to_dsl: None,
    };
    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".to_string()
        )))
        .is_ok()
    );
    let err = attr
        .validate(&Value::Concrete(ConcreteValue::String(
            "us-east-1".to_string(),
        )))
        .unwrap_err();
    match err {
        TypeError::ValidationFailed { message } => {
            assert!(message.contains("ap-northeast-1") && message.contains("us-east-1"));
        }
        other => panic!("expected ValidationFailed, got: {other:?}"),
    }
}

#[test]
fn custom_validator_returns_structured_type_error_directly() {
    // The validator returns `TypeError::InvalidEnumVariant` directly,
    // bypassing the legacy `String -> ValidationFailed` round-trip.
    // This is what unlocks LSP code-action quick-fixes for Custom-typed
    // attributes (see #2220 / #2309 for the structured-error path).
    let attr = AttributeType::Custom {
        semantic_name: Some("Mode".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: validator(|v| match v {
            Value::Concrete(ConcreteValue::String(s)) if s == "fast" || s == "slow" => Ok(()),
            Value::Concrete(ConcreteValue::String(s)) => Err(TypeError::InvalidEnumVariant {
                value: s.clone(),
                attribute: None,
                type_name: Some("Mode".to_string()),
                expected: vec![
                    ExpectedEnumVariant::from_namespaced(None, "Mode", "fast", false),
                    ExpectedEnumVariant::from_namespaced(None, "Mode", "slow", false),
                ],
            }),
            other => Err(TypeError::TypeMismatch {
                expected: "String".to_string(),
                got: other.type_name(),
            }),
        }),
        namespace: None,
        to_dsl: None,
    };
    let err = attr
        .validate(&Value::Concrete(ConcreteValue::String(
            "medium".to_string(),
        )))
        .unwrap_err();
    match err {
        TypeError::InvalidEnumVariant {
            value,
            type_name,
            expected,
            ..
        } => {
            assert_eq!(value, "medium");
            assert_eq!(type_name.as_deref(), Some("Mode"));
            let values: Vec<&str> = expected.iter().map(|e| e.value.as_str()).collect();
            assert_eq!(values, vec!["fast", "slow"]);
        }
        other => panic!("expected InvalidEnumVariant, got: {other:?}"),
    }
}

// --- SchemaRegistry tests (#2328) ---

#[test]
fn schema_registry_new_is_empty() {
    let registry = SchemaRegistry::new();
    assert_eq!(registry.len(), 0);
    assert!(registry.is_empty());
}

#[test]
fn schema_registry_inserts_managed_and_data_source_for_same_type() {
    let mut registry = SchemaRegistry::new();
    let managed = ResourceSchema::new("s3.Bucket");
    let data_source = ResourceSchema::new("s3.Bucket").as_data_source();

    registry.insert("aws", managed);
    registry.insert("aws", data_source);

    assert_eq!(registry.len(), 2);
    assert!(registry.has_managed("aws", "s3.Bucket"));
    assert!(registry.has_data_source("aws", "s3.Bucket"));
}

#[test]
fn schema_registry_get_picks_kind_from_resource() {
    use crate::resource::Resource;

    let mut registry = SchemaRegistry::new();
    registry.insert("aws", ResourceSchema::new("s3.Bucket"));
    registry.insert("aws", ResourceSchema::new("s3.Bucket").as_data_source());

    let managed_res = Resource::with_provider("aws", "s3.Bucket", "new", None);
    let data_res = Resource::with_provider("aws", "s3.Bucket", "existing", None)
        .with_kind(crate::resource::ResourceKind::DataSource);

    let m = registry
        .get_for(&managed_res)
        .expect("managed schema present");
    assert_eq!(m.kind, SchemaKind::Managed);

    let d = registry
        .get_for(&data_res)
        .expect("data source schema present");
    assert_eq!(d.kind, SchemaKind::DataSource);
}

#[test]
fn schema_registry_missing_returns_none() {
    let registry = SchemaRegistry::new();
    assert!(
        registry
            .get("aws", "s3.Bucket", SchemaKind::Managed)
            .is_none()
    );
    assert!(!registry.has_managed("aws", "s3.Bucket"));
    assert!(!registry.has_data_source("aws", "s3.Bucket"));
}

#[test]
fn schema_registry_has_managed_only_does_not_imply_data_source() {
    let mut registry = SchemaRegistry::new();
    registry.insert("aws", ResourceSchema::new("s3.Bucket"));

    assert!(registry.has_managed("aws", "s3.Bucket"));
    assert!(!registry.has_data_source("aws", "s3.Bucket"));
}

#[test]
fn validate_skips_value_unknown_for_primitive_types() {
    // `Value::Deferred(DeferredValue::Unknown)` carries no concrete type at plan time, so it
    // takes the same skip path as `FunctionCall` and `Secret`. Without
    // this, a `for x in upstream.list { ... attr = x ... }` body fails
    // parse-time validation with `expected <type>, got unknown`.
    use crate::resource::{AccessPath, UnknownReason};
    let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
    assert!(AttributeType::String.validate(&unknown).is_ok());
    assert!(AttributeType::Int.validate(&unknown).is_ok());
    assert!(AttributeType::Bool.validate(&unknown).is_ok());

    let upstream = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
        path: AccessPath::with_fields("net", "vpc", vec!["vpc_id".into()]),
    }));
    assert!(AttributeType::String.validate(&upstream).is_ok());
    assert!(AttributeType::Int.validate(&upstream).is_ok());
}

#[test]
fn walk_custom_lookup_skips_value_unknown() {
    // Custom-typed attributes (e.g. AWS resource-id types like `vpc_id`)
    // run through `walk_custom_lookup` instead of `validate`. Skip
    // `Value::Deferred(DeferredValue::Unknown)` here too, otherwise a custom-typed attribute
    // bound from a for-expression element fails plan with
    // `expected vpc_id, got unknown`.
    use crate::resource::UnknownReason;

    let always_fail = validator(|_v: &Value| {
        Err(TypeError::ValidationFailed {
            message: "validator must not run for Value::Deferred(DeferredValue::Unknown)"
                .to_string(),
        })
    });
    let custom_type = AttributeType::Custom {
        base: Box::new(AttributeType::String),
        validate: always_fail,
        semantic_name: Some("vpc_id".to_string()),
        namespace: None,
        pattern: None,
        length: None,
        to_dsl: None,
    };

    let mut errors = Vec::new();
    walk_custom_lookup(
        &custom_type,
        &Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
        "vpc_id",
        &|_, _| {
            Err(TypeError::ValidationFailed {
                message: "should never be called".to_string(),
            })
        },
        &mut errors,
    );
    assert!(
        errors.is_empty(),
        "Value::Deferred(DeferredValue::Unknown) must not invoke the custom validator, got: {errors:?}"
    );
}

// =====================================================================
// awscc#217: `walk_custom_lookup` walked every Union member and pushed
// every member's failure into the shared errors vec. A value that
// validly matched one arm (an IPv4 CIDR like `10.0.0.0/8` matching the
// `ipv4_cidr()` arm of `types::cidr()`) still surfaced the sibling
// IPv6 arm's failure (`expected 8 groups, got 1`). Union semantics is
// "any member accepts" — if one member emits no errors, the union
// succeeds and sibling failures are discarded.
// =====================================================================

#[test]
fn union_walk_custom_lookup_succeeds_when_any_member_accepts() {
    // Custom-typed Union members: the host-side lookup approves one
    // arm by name and rejects the other. The Union must succeed
    // because the approving arm matches; the sibling's failure is
    // discarded.
    let custom = |name: &str| AttributeType::Custom {
        base: Box::new(AttributeType::String),
        validate: validator(|_v: &Value| Ok(())),
        semantic_name: Some(name.to_string()),
        namespace: None,
        pattern: None,
        length: None,
        to_dsl: None,
    };
    let union = AttributeType::Union(vec![custom("ok_arm"), custom("fail_arm")]);

    let lookup = |name: &str, _v: &Value| -> Result<(), TypeError> {
        if name == "ok_arm" {
            Ok(())
        } else {
            Err(TypeError::ValidationFailed {
                message: format!("{name} rejects"),
            })
        }
    };

    let mut errors = Vec::new();
    walk_custom_lookup(
        &union,
        &Value::Concrete(ConcreteValue::String("anything".to_string())),
        "attr",
        &lookup,
        &mut errors,
    );
    assert!(
        errors.is_empty(),
        "Union must succeed when any member accepts; got errors: {errors:?}"
    );
}

#[test]
fn union_walk_custom_lookup_emits_smallest_error_set_when_all_fail() {
    // Both arms fail. The Union must surface only one of the
    // sibling failure sets (the smaller one if they differ), not the
    // sum. Both arms emit a single error here, so the Union's error
    // count must be 1, not 2.
    let custom = |name: &str| AttributeType::Custom {
        base: Box::new(AttributeType::String),
        validate: validator(|_v: &Value| Ok(())),
        semantic_name: Some(name.to_string()),
        namespace: None,
        pattern: None,
        length: None,
        to_dsl: None,
    };
    let union = AttributeType::Union(vec![custom("a"), custom("b")]);

    let lookup = |name: &str, _v: &Value| -> Result<(), TypeError> {
        Err(TypeError::ValidationFailed {
            message: format!("{name} rejects"),
        })
    };

    let mut errors = Vec::new();
    walk_custom_lookup(
        &union,
        &Value::Concrete(ConcreteValue::String("anything".to_string())),
        "attr",
        &lookup,
        &mut errors,
    );
    assert_eq!(
        errors.len(),
        1,
        "Union must surface only the closest near-match, not the sum of all members"
    );
}

#[test]
fn union_walk_custom_lookup_cidr_accepts_ipv4_when_lookup_routes_correctly() {
    // Regression for awscc#217. WASM-plugin schemas arrive with the
    // schema-attached `Custom.validate` closure stripped (replaced by
    // `noop_validator`); host-side validation runs through `lookup`
    // by semantic_name. Models that: lookup approves `Ipv4Cidr` for
    // `"10.0.0.0/8"` and rejects `Ipv6Cidr`. The Union must surface
    // no errors — the previous loop would have pushed the IPv6
    // rejection through anyway. This is the awscc#217 reproduction.
    let cidr = AttributeType::Union(vec![types::ipv4_cidr(), types::ipv6_cidr()]);

    let lookup = |name: &str, value: &Value| -> Result<(), TypeError> {
        let Value::Concrete(ConcreteValue::String(s)) = value else {
            return Ok(());
        };
        match name {
            "Ipv4Cidr" => {
                validate_ipv4_cidr(s).map_err(|message| TypeError::ValidationFailed { message })
            }
            "Ipv6Cidr" => {
                validate_ipv6_cidr(s).map_err(|message| TypeError::ValidationFailed { message })
            }
            _ => Ok(()),
        }
    };

    let mut errors = Vec::new();
    walk_custom_lookup(
        &cidr,
        &Value::Concrete(ConcreteValue::String("10.0.0.0/8".to_string())),
        "cidr",
        &lookup,
        &mut errors,
    );
    assert!(
        errors.is_empty(),
        "Union<ipv4_cidr, ipv6_cidr> must accept `10.0.0.0/8`; got: {errors:?}"
    );
}

// =====================================================================
// carina#2831: `StringEnum.dsl_aliases` is the data form that survives
// the WASM-component boundary, replacing the closure-based `to_dsl`
// pointer that could not. The validator must accept both the API
// spelling and every DSL alias, including in fully-qualified form
// (`awscc.<service>.<TypeName>.<dsl_spelling>`), and must list both
// in the diagnostic for an unknown value.
// =====================================================================

#[test]
fn dsl_aliases_validator_accepts_dsl_spellings_only() {
    // Updated for carina#2980: once `dsl_aliases` is non-empty for an
    // enum, the validator is strict — only the DSL (snake_case) side
    // of an `(api, dsl)` pair is accepted on input. Pre-#2980 the
    // bare API spelling (`BucketOwnerEnforced`) was also accepted;
    // that path is gone so users cannot ship fixtures using the AWS
    // canonical spelling and have CI pass anyway.
    //
    // Original awscc#199 / aws#247 case still works: the fully- and
    // bare-DSL forms (`bucket_owner_enforced`,
    // `awscc.s3.Bucket.ObjectOwnership.bucket_owner_enforced`) are
    // accepted.
    let t = AttributeType::StringEnum {
        name: "ObjectOwnership".to_string(),
        values: vec![
            "ObjectWriter".to_string(),
            "BucketOwnerPreferred".to_string(),
            "BucketOwnerEnforced".to_string(),
        ],
        namespace: Some("awscc.s3.Bucket".to_string()),
        dsl_aliases: vec![
            ("ObjectWriter".to_string(), "object_writer".to_string()),
            (
                "BucketOwnerPreferred".to_string(),
                "bucket_owner_preferred".to_string(),
            ),
            (
                "BucketOwnerEnforced".to_string(),
                "bucket_owner_enforced".to_string(),
            ),
        ],
    };

    // Bare API spelling: REJECTED under strict mode (the DSL alias
    // rewrite invalidates the API spelling — users must type the DSL
    // form `bucket_owner_enforced`). Phase 4 of carina#2986 carries the
    // input as `EnumIdentifier` because the user wrote it as a bare
    // identifier, not a quoted string.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "BucketOwnerEnforced".to_string(),
        )))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("bucket_owner_enforced"),
        "diagnostic must point users at the DSL spelling, got: {msg}"
    );

    // Bare DSL spelling: accepted (alias).
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "bucket_owner_enforced".to_string()
        )))
        .is_ok()
    );
    // Fully-qualified API spelling: REJECTED under strict mode.
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "awscc.s3.Bucket.ObjectOwnership.BucketOwnerEnforced".to_string()
        )))
        .is_err()
    );
    // Fully-qualified DSL spelling: accepted (the awscc#199 case).
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "awscc.s3.Bucket.ObjectOwnership.bucket_owner_enforced".to_string()
        )))
        .is_ok()
    );

    // An unrelated value: rejected, with both spellings still
    // listed for context (the API spelling is what the AWS docs show,
    // the DSL spelling is what `validate` accepts).
    let err = t
        .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "garbage".to_string(),
        )))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("BucketOwnerEnforced"),
        "diagnostic must list the API spelling, got: {msg}"
    );
    assert!(
        msg.contains("bucket_owner_enforced"),
        "diagnostic must list the DSL spelling, got: {msg}"
    );
}

#[test]
fn enum_without_dsl_aliases_accepts_api_spelling_as_before() {
    // An enum where codegen has not yet populated `dsl_aliases`
    // (e.g. a newly added resource pending the carina#2980 sweep)
    // keeps the pre-#2980 behavior: API canonical spellings are
    // accepted via the `values` list. This is the staged-migration
    // hook — strictness flips on per enum as codegen populates the
    // table, so a partial sweep never breaks compilation.
    let t = AttributeType::StringEnum {
        name: "TrafficType".to_string(),
        values: vec![
            "ACCEPT".to_string(),
            "REJECT".to_string(),
            "ALL".to_string(),
        ],
        namespace: Some("aws.ec2.FlowLog".to_string()),
        dsl_aliases: vec![],
    };
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "ALL".to_string()
        )))
        .is_ok(),
        "lax mode (empty dsl_aliases) accepts API canonical"
    );
}

#[test]
fn dsl_aliases_diagnostic_tags_alias_entries_distinct_from_canonical() {
    // The `expected` list carried by `TypeError::InvalidEnumVariant`
    // tags each entry with `is_alias`, so an LSP code action can
    // suggest the canonical form. Pin the tagging.
    let t = AttributeType::StringEnum {
        name: "VersioningStatus".to_string(),
        values: vec!["Enabled".to_string(), "Suspended".to_string()],
        namespace: Some("aws.s3.Bucket".to_string()),
        dsl_aliases: vec![
            ("Enabled".to_string(), "enabled".to_string()),
            ("Suspended".to_string(), "suspended".to_string()),
        ],
    };
    // Use `EnumIdentifier` so the strict-mode validator reaches the
    // unknown-variant branch (the alias-tagging behavior we want to
    // pin). A `String` here would take the `StringLiteralExpectedEnum`
    // path which is covered by sibling tests.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "zzz".to_string(),
        )))
        .unwrap_err();
    let TypeError::InvalidEnumVariant { expected, .. } = err else {
        panic!("expected InvalidEnumVariant");
    };
    let canonical: Vec<&str> = expected
        .iter()
        .filter(|e| !e.is_alias)
        .map(|e| e.value.as_str())
        .collect();
    let aliases: Vec<&str> = expected
        .iter()
        .filter(|e| e.is_alias)
        .map(|e| e.value.as_str())
        .collect();
    assert_eq!(canonical, vec!["Enabled", "Suspended"]);
    assert_eq!(aliases, vec!["enabled", "suspended"]);
}

#[test]
fn dsl_aliases_empty_keeps_api_only_validation() {
    // An empty `dsl_aliases` is the wire-format default for older
    // providers and for enums whose API spelling already matches the
    // DSL spelling. Validation must continue to accept the API
    // spelling and nothing else.
    let t = AttributeType::StringEnum {
        name: "Status".to_string(),
        values: vec!["active".to_string(), "inactive".to_string()],
        namespace: Some("test.r".to_string()),
        dsl_aliases: vec![],
    };
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "active".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::EnumIdentifier(
            "nope".to_string()
        )))
        .is_err()
    );
}

// #2978 — collision between a `let` binding name and a `StringEnum`
// DSL alias. The parser resolves a bare identifier to the binding
// first (see parser/expressions/primary.rs:367-374), producing a
// `Value::Deferred(DeferredValue::BindingRef { binding: "vpc" })` for
// the attribute value. Without the dedicated check, the deferred value
// flows past `validate` and only surfaces later as a `${vpc}`-style
// error from the resolver, which is hard to diagnose. Pin the
// behavior here so a regression on the `validate` side is visible
// without round-tripping through plan execution.
mod string_enum_binding_collision {
    use super::*;
    use crate::resource::DeferredValue;

    fn flow_log_resource_type() -> AttributeType {
        // Mirrors the awscc.ec2.FlowLog.ResourceType enum that
        // surfaced the issue. The `"vpc"` alias is what collides with
        // a `let vpc = ...` binding name in real fixtures.
        AttributeType::StringEnum {
            name: "ResourceType".to_string(),
            values: vec![
                "NetworkInterface".to_string(),
                "Subnet".to_string(),
                "VPC".to_string(),
            ],
            namespace: Some("awscc.ec2.FlowLog".to_string()),
            dsl_aliases: vec![
                (
                    "NetworkInterface".to_string(),
                    "network_interface".to_string(),
                ),
                ("Subnet".to_string(), "subnet".to_string()),
                ("VPC".to_string(), "vpc".to_string()),
            ],
        }
    }

    #[test]
    fn bare_binding_matching_dsl_alias_is_rejected() {
        let t = flow_log_resource_type();
        // `let vpc = ...` in scope → parser hands the validator a
        // `BindingRef("vpc")` for `resource_type = vpc`.
        let value = Value::Deferred(DeferredValue::BindingRef {
            binding: "vpc".to_string(),
        });
        let err = t.validate(&value).unwrap_err();
        let TypeError::ValidationFailed { message } = err else {
            panic!("expected ValidationFailed, got {:?}", err);
        };
        assert!(
            message.contains("shadowed by a `let` binding"),
            "message did not mention the shadowing rule: {message}"
        );
        // Suggest the type-qualified form using the DSL spelling.
        assert!(
            message.contains("ResourceType.vpc"),
            "missing type-qualified suggestion: {message}"
        );
        // And the fully qualified form.
        assert!(
            message.contains("awscc.ec2.FlowLog.ResourceType.vpc"),
            "missing fully-qualified suggestion: {message}"
        );
        // And the quoted-string escape hatch.
        assert!(
            message.contains("'vpc'"),
            "missing quoted-string suggestion: {message}"
        );
    }

    #[test]
    fn bare_binding_matching_canonical_value_is_rejected() {
        // Same trap, hit through the canonical spelling rather than
        // the alias: `let VPC = ...` would collide with the `"VPC"`
        // value directly.
        let t = flow_log_resource_type();
        let value = Value::Deferred(DeferredValue::BindingRef {
            binding: "VPC".to_string(),
        });
        let err = t.validate(&value).unwrap_err();
        let TypeError::ValidationFailed { message } = err else {
            panic!("expected ValidationFailed, got {:?}", err);
        };
        assert!(message.contains("shadowed by a `let` binding"), "{message}");
    }

    #[test]
    fn bare_binding_not_matching_alias_passes_through_validate() {
        // `let some_other_name = ...` does not match any DSL alias for
        // ResourceType, so it is not the collision case. `validate`
        // must stay quiet at this layer; the deferred-aware resolver
        // can still surface other errors downstream.
        let t = flow_log_resource_type();
        let value = Value::Deferred(DeferredValue::BindingRef {
            binding: "some_other_name".to_string(),
        });
        assert!(t.validate(&value).is_ok());
    }

    #[test]
    fn collision_check_only_fires_on_string_enum() {
        // Other AttributeType kinds (e.g., String) must not be
        // affected by the collision check — many of them legitimately
        // accept a `BindingRef`.
        let t = AttributeType::String;
        let value = Value::Deferred(DeferredValue::BindingRef {
            binding: "vpc".to_string(),
        });
        assert!(t.validate(&value).is_ok());
    }

    #[test]
    fn explicit_quoted_string_is_rejected() {
        // Phase 4 of carina#2986: `attribute = "vpc"` reaches the
        // validator as a `ConcreteValue::String` (the user wrote a
        // quoted literal). Strict mode rejects it with
        // `StringLiteralExpectedEnum` — the message must point users
        // at the DSL identifier form.
        let t = flow_log_resource_type();
        let value = Value::Concrete(ConcreteValue::String("vpc".to_string()));
        let err = t.validate(&value).unwrap_err();
        assert!(
            matches!(err, TypeError::StringLiteralExpectedEnum { ref user_typed, .. } if user_typed == "vpc"),
            "expected StringLiteralExpectedEnum, got: {err:?}"
        );
    }

    #[test]
    fn bare_identifier_form_passes() {
        // `attribute = vpc` (bare identifier; no binding in scope)
        // reaches the validator as `ConcreteValue::EnumIdentifier("vpc")`
        // — the validator resolves it against the `dsl_aliases` table
        // for `ResourceType` and accepts.
        let t = flow_log_resource_type();
        let value = Value::Concrete(ConcreteValue::EnumIdentifier("vpc".to_string()));
        assert!(t.validate(&value).is_ok());
    }

    #[test]
    fn fully_qualified_form_still_passes() {
        // `attribute = awscc.ec2.FlowLog.ResourceType.vpc` reaches the
        // validator as `ConcreteValue::EnumIdentifier` (carina#2986
        // Phase 3 parser routing).
        let t = flow_log_resource_type();
        let value = Value::Concrete(ConcreteValue::EnumIdentifier(
            "awscc.ec2.FlowLog.ResourceType.vpc".to_string(),
        ));
        assert!(t.validate(&value).is_ok());
    }
}

mod dsl_map_api_for {
    use super::*;

    fn alias_table() -> Vec<(String, String)> {
        vec![
            ("Enabled".to_string(), "enabled".to_string()),
            ("Suspended".to_string(), "suspended".to_string()),
            ("VPC".to_string(), "vpc".to_string()),
        ]
    }

    #[test]
    fn aliases_resolves_dsl_to_api_canonical() {
        let aliases = alias_table();
        let map = DslMap::Aliases(&aliases);
        assert_eq!(map.api_for("enabled"), "Enabled");
        assert_eq!(map.api_for("suspended"), "Suspended");
        assert_eq!(map.api_for("vpc"), "VPC");
    }

    #[test]
    fn aliases_returns_input_when_no_match() {
        // No alias entry for `unknown`; passthrough so callers can hand
        // off identity-mapped values (where API spelling == DSL spelling)
        // straight to the SDK.
        let aliases = alias_table();
        let map = DslMap::Aliases(&aliases);
        assert_eq!(map.api_for("unknown"), "unknown");
    }

    #[test]
    fn aliases_returns_input_when_given_api_canonical() {
        // If the caller already has the API spelling, `api_for` is a
        // no-op: the alias table is `(api, dsl)`, so an API value
        // doesn't match any `dsl` side and falls through.
        let aliases = alias_table();
        let map = DslMap::Aliases(&aliases);
        assert_eq!(map.api_for("Enabled"), "Enabled");
    }

    #[test]
    fn aliases_empty_table_is_identity() {
        let aliases: Vec<(String, String)> = vec![];
        let map = DslMap::Aliases(&aliases);
        assert_eq!(map.api_for("anything"), "anything");
    }

    #[test]
    fn closure_some_returns_input_unchanged() {
        // The closure is one-way (api -> dsl); reversing it is not
        // representable, so `api_for` is documented to return the input
        // as-is for the Closure variant. Callers that go through a
        // `Closure` (currently only Region with hyphen↔underscore) must
        // reverse the mapping themselves.
        fn to_dsl(api: &str) -> String {
            api.replace('-', "_")
        }
        let map = DslMap::Closure(Some(to_dsl));
        assert_eq!(map.api_for("ap_northeast_1"), "ap_northeast_1");
    }

    #[test]
    fn closure_none_returns_input_unchanged() {
        let map = DslMap::Closure(None);
        assert_eq!(map.api_for("anything"), "anything");
    }

    #[test]
    fn aliases_duplicate_dsl_spelling_returns_first_match() {
        // Pins the deterministic behavior when two entries share a DSL
        // spelling: `find_map` returns the first match. Such duplicates
        // should not exist in real codegen output, but the inverse map
        // is intrinsically lossy and the documented contract is "first
        // wins" rather than "panic" or "undefined".
        let aliases = vec![
            ("Foo".to_string(), "bar".to_string()),
            ("Baz".to_string(), "bar".to_string()),
        ];
        let map = DslMap::Aliases(&aliases);
        assert_eq!(map.api_for("bar"), "Foo");
    }
}

// carina#2996: `Map<StringEnum, V>` bare-identifier key acceptance.
fn condition_operator_map(value: AttributeType) -> AttributeType {
    AttributeType::Map {
        key: Box::new(AttributeType::StringEnum {
            name: "ConditionOperator".to_string(),
            values: vec![
                "string_equals".to_string(),
                "string_not_equals".to_string(),
                "arn_like".to_string(),
            ],
            namespace: Some("aws.iam.ConditionOperator".to_string()),
            dsl_aliases: vec![],
        }),
        value: Box::new(value),
    }
}

fn map_value_with_one_key(key: &str) -> Value {
    let mut inner: IndexMap<String, Value> = IndexMap::new();
    inner.insert(
        key.to_string(),
        Value::Concrete(ConcreteValue::String("v".to_string())),
    );
    Value::Concrete(ConcreteValue::Map(inner))
}

#[test]
fn validate_map_with_string_enum_key_accepts_bare_identifier_spelling() {
    let map_t = condition_operator_map(AttributeType::String);
    let val = map_value_with_one_key("string_equals");
    assert!(
        map_t.validate(&val).is_ok(),
        "bare-identifier map key for Map<StringEnum, V> must be accepted: {:?}",
        map_t.validate(&val)
    );
}

#[test]
fn validate_map_with_string_enum_key_accepts_dsl_alias_spelling() {
    // `IpProtocol` has a `("-1", "all")` alias: DSL must accept `all`
    // both at attribute-value position (already covered) and at
    // map-key position when used as `Map<StringEnum<IpProtocol>, V>`.
    let map_t = AttributeType::Map {
        key: Box::new(AttributeType::StringEnum {
            name: "IpProtocol".to_string(),
            values: vec!["tcp".to_string(), "-1".to_string()],
            namespace: Some("awscc.ec2.SecurityGroup".to_string()),
            dsl_aliases: vec![("-1".to_string(), "all".to_string())],
        }),
        value: Box::new(AttributeType::String),
    };
    let val = map_value_with_one_key("all");
    assert!(
        map_t.validate(&val).is_ok(),
        "DSL-alias map key spelling must be accepted: {:?}",
        map_t.validate(&val)
    );
}

#[test]
fn validate_map_with_string_enum_key_rejects_unknown_variant() {
    let map_t = condition_operator_map(AttributeType::String);
    let val = map_value_with_one_key("not_a_variant");
    // The diagnostic must surface as `MapKeyError(InvalidEnumVariant)`,
    // not the `StringLiteralExpectedEnum` shape that misled the user
    // in carina#2996.
    match map_t.validate(&val).unwrap_err() {
        TypeError::MapKeyError { key, inner } => {
            assert_eq!(key, "not_a_variant");
            assert!(
                matches!(*inner, TypeError::InvalidEnumVariant { .. }),
                "expected InvalidEnumVariant inside MapKeyError, got: {inner:?}"
            );
        }
        other => panic!("expected MapKeyError, got: {other:?}"),
    }
}
