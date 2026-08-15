use super::*;

#[test]
fn attribute_schema_write_only_default_false() {
    let attr = AttributeSchema::new("ipv4_netmask_length", AttributeType::int());
    assert!(!attr.write_only);
}

#[test]
fn attribute_schema_write_only_builder() {
    let attr = AttributeSchema::new("ipv4_netmask_length", AttributeType::int()).write_only();
    assert!(attr.write_only);
}

#[test]
fn resource_schema_kind_default_managed() {
    let schema = ResourceSchema::new("test.resource");
    assert_eq!(schema.kind, SchemaKind::Resource);
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
    let t = AttributeType::string();
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
fn validate_enum_type() {
    // Phase 4 of carina#2986: enum attributes accept only
    // `ConcreteValue::EnumIdentifier`. Constructed-by-hand strings are
    // rejected as `StringLiteralExpectedEnum` — see
    // `validate_enum_rejects_quoted_string_literal` for that path.
    let t = AttributeType::enum_(
        crate::schema::enum_identity("AddressFamily", Some("awscc.ec2.ipam_pool")),
        Some(vec!["IPv4".to_string(), "IPv6".to_string()]),
        vec![],
        None,
        None,
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.ec2.ipam_pool.AddressFamily.IPv4".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "IPv6".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "ipv4".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "IPv5".to_string()
        )))
        .is_err()
    );
}

#[test]
fn validate_enum_rejects_quoted_string_literal() {
    // Phase 4 of carina#2986: a `String`-shaped value on a `Enum`
    // attribute means the user wrote `attr = "value"`. The validator
    // emits `StringLiteralExpectedEnum` with the full expected variant
    // list so the LSP code action can offer "drop quotes / use
    // identifier form" without re-deriving candidates.
    let t = AttributeType::enum_(
        crate::schema::enum_identity("AddressFamily", Some("awscc.ec2.ipam_pool")),
        Some(vec!["IPv4".to_string(), "IPv6".to_string()]),
        vec![],
        None,
        None,
    );
    let err = t
        .validate(&Value::Concrete(ConcreteValue::String("IPv4".to_string())))
        .unwrap_err();
    assert!(
        matches!(err, TypeError::StringLiteralExpectedEnum { ref user_typed, .. } if user_typed == "IPv4"),
        "expected StringLiteralExpectedEnum, got: {err:?}"
    );
}

#[test]
fn enum_type_name_uses_dotted_identity() {
    let t = AttributeType::enum_(
        crate::schema::enum_identity("VersioningStatus", Some("aws.s3.Bucket")),
        Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
        vec![],
        None,
        None,
    );
    assert_eq!(t.type_name(), "aws.s3.Bucket.VersioningStatus");
}

#[test]
fn validate_enum_accepts_dsl_alias() {
    let t = AttributeType::enum_(
        crate::schema::enum_identity("IpProtocol", Some("awscc.ec2.SecurityGroup")),
        Some(vec![
            "tcp".to_string(),
            "udp".to_string(),
            "icmp".to_string(),
            "icmpv6".to_string(),
            "-1".to_string(),
        ]),
        vec![("-1".to_string(), "all".to_string())],
        None,
        None,
    );
    // Canonical "-1" is rewritten to DSL "all" — the DSL surface must
    // not accept the API form when an alias is registered. Users
    // write `all`. Updated for carina#2980 / carina#2986.
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "-1".to_string()
        )))
        .is_err()
    );
    // DSL alias "all" should be accepted
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.ec2.SecurityGroup.IpProtocol.all".to_string()
        )))
        .is_ok()
    );
    // Other canonical values without an alias rewrite (e.g. `tcp`,
    // which is already snake_case) keep working — the API spelling
    // *is* the DSL spelling for them.
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "tcp".to_string()
        )))
        .is_ok()
    );
    // Invalid values should still be rejected
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "invalid".to_string()
        )))
        .is_err()
    );
}

fn iam_policy_version_enum() -> AttributeType {
    AttributeType::enum_(
        crate::schema::enum_identity("Version", Some("aws.iam.PolicyDocument")),
        Some(vec!["2012-10-17".to_string(), "2008-10-17".to_string()]),
        vec![
            ("2012-10-17".to_string(), "2012_10_17".to_string()),
            ("2008-10-17".to_string(), "2008_10_17".to_string()),
        ],
        None,
        None,
    )
}

fn assert_iam_policy_version_candidates(expected: &[ExpectedEnumVariant]) {
    let rendered: Vec<String> = expected.iter().map(ToString::to_string).collect();
    assert_eq!(
        rendered,
        vec![
            "aws.iam.PolicyDocument.Version.2012_10_17",
            "aws.iam.PolicyDocument.Version.2008_10_17",
        ]
    );
    assert!(
        rendered.iter().all(|candidate| !candidate.contains('-')),
        "Version candidates must use DSL spellings only, got: {rendered:?}"
    );
    assert!(
        expected.iter().all(|candidate| !candidate.is_alias),
        "1:1 API-to-DSL alias tables must collapse to canonical DSL candidates: {expected:?}"
    );
}

#[test]
fn invalid_enum_variant_candidates_use_dsl_spelling_for_enum_aliases() {
    let err = iam_policy_version_enum()
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "bad_version".to_string(),
        )))
        .unwrap_err();
    let TypeError::InvalidEnumVariant { expected, .. } = err else {
        panic!("expected InvalidEnumVariant, got {err:?}");
    };

    assert_iam_policy_version_candidates(&expected);
    let msg = TypeError::InvalidEnumVariant {
        value: "bad_version".to_string(),
        attribute: None,
        type_name: Some("Version".to_string()),
        expected,
    }
    .to_string();
    assert!(msg.contains("aws.iam.PolicyDocument.Version.2012_10_17"));
    assert!(
        !msg.contains("2012-10-17"),
        "message leaked API spelling: {msg}"
    );
    assert!(
        !msg.contains("2008-10-17"),
        "message leaked API spelling: {msg}"
    );
}

#[test]
fn string_literal_expected_enum_candidates_use_dsl_spelling_for_enum_aliases() {
    let err = iam_policy_version_enum()
        .validate(&Value::Concrete(ConcreteValue::String(
            "2012-10-17".to_string(),
        )))
        .unwrap_err();
    let TypeError::StringLiteralExpectedEnum { expected, .. } = err else {
        panic!("expected StringLiteralExpectedEnum, got {err:?}");
    };

    assert_iam_policy_version_candidates(&expected);
    let msg = TypeError::StringLiteralExpectedEnum {
        user_typed: "2012-10-17".to_string(),
        attribute: None,
        type_name: "Version".to_string(),
        expected,
        extra_message: None,
    }
    .to_string();
    assert!(msg.contains("aws.iam.PolicyDocument.Version.2012_10_17"));
    assert!(
        !msg.contains("2008-10-17"),
        "message leaked API spelling: {msg}"
    );
}

#[test]
fn enum_candidates_preserve_genuine_extra_aliases() {
    let t = AttributeType::enum_(
        crate::schema::enum_identity("Mode", Some("test.service.Resource")),
        Some(vec!["ALL".to_string()]),
        vec![
            ("ALL".to_string(), "all".to_string()),
            ("LEGACY_ALL".to_string(), "legacy_all".to_string()),
        ],
        None,
        None,
    );
    let err = t
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "bad_mode".to_string(),
        )))
        .unwrap_err();
    let TypeError::InvalidEnumVariant { expected, .. } = err else {
        panic!("expected InvalidEnumVariant, got {err:?}");
    };

    let rendered: Vec<String> = expected.iter().map(ToString::to_string).collect();
    assert_eq!(
        rendered,
        vec![
            "test.service.Resource.Mode.all",
            "test.service.Resource.Mode.legacy_all",
        ]
    );
    assert!(!expected[0].is_alias);
    assert!(expected[1].is_alias);
}

#[test]
fn validate_enum_all_without_dsl_aliases_requires_explicit_variant() {
    // Without "all" as a direct variant or in `dsl_aliases`, the bare
    // identifier `all` is not accepted. Issue #1428.
    let without_all = AttributeType::enum_(
        TypeIdentity::bare(String::new()),
        Some(vec![
            "tcp".to_string(),
            "udp".to_string(),
            "icmp".to_string(),
            "icmpv6".to_string(),
            "-1".to_string(),
        ]),
        vec![],
        None,
        None,
    );
    // Without "all" in values and no dsl_aliases entry mapping to "all", it is rejected
    assert!(
        without_all
            .validate(&Value::Concrete(ConcreteValue::enum_identifier(
                "all".to_string()
            )))
            .is_err()
    );

    // With "all" added to values, it is accepted even without dsl_aliases
    let with_all = AttributeType::enum_(
        TypeIdentity::bare(String::new()),
        Some(vec![
            "tcp".to_string(),
            "udp".to_string(),
            "icmp".to_string(),
            "icmpv6".to_string(),
            "-1".to_string(),
            "all".to_string(),
        ]),
        vec![],
        None,
        None,
    );
    assert!(
        with_all
            .validate(&Value::Concrete(ConcreteValue::enum_identifier(
                "all".to_string()
            )))
            .is_ok()
    );
}

#[test]
fn validate_enum_accepts_values_with_dots() {
    // Values like "ipsec.1" contain dots that should not be treated as
    // namespace separators (issue #611)
    let t = AttributeType::enum_(
        crate::schema::enum_identity("Type", Some("awscc.ec2.vpn_gateway")),
        Some(vec!["ipsec.1".to_string()]),
        vec![],
        None,
        None,
    );
    // Bare identifier with dot should match directly (carried as
    // `EnumIdentifier` under strict mode — the test name still says
    // "Quoted string" for historical reasons but values are written
    // unquoted in real DSL).
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "ipsec.1".to_string()
        )))
        .is_ok()
    );
    // Fully qualified form should also be accepted
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
        )))
        .is_ok()
    );
    // Invalid value should still be rejected
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
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
    let t = AttributeType::enum_(
        crate::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
        Some(vec!["AWS_ACCOUNT".to_string()]),
        vec![],
        None,
        None,
    );
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
    let t = AttributeType::enum_(
        crate::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
        Some(vec!["AWS_ACCOUNT".to_string()]),
        vec![],
        None,
        None,
    );
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
    let t = AttributeType::enum_(
        crate::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
        Some(vec!["AWS_ACCOUNT".to_string()]),
        vec![],
        None,
        None,
    );
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
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
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
fn custom_constraint_errors_format_type_and_attribute_context() {
    let pattern = TypeError::PatternMismatch {
        value: "ABC".to_string(),
        pattern: "^[a-z]+$".to_string(),
        attribute: None,
        type_name: Some("EntityDescription".to_string()),
    };
    assert_eq!(
        pattern.to_string(),
        "Invalid EntityDescription value 'ABC': does not match required pattern /^[a-z]+$/"
    );
    assert_eq!(
        pattern.with_attribute("description").to_string(),
        "Invalid value 'ABC' for 'description' (EntityDescription): does not match required pattern /^[a-z]+$/"
    );

    let length = TypeError::LengthOutOfRange {
        value: "".to_string(),
        length: 0,
        min: Some(1),
        max: None,
        attribute: None,
        type_name: Some("EntityDescription".to_string()),
    };
    assert_eq!(
        length.with_attribute("description").to_string(),
        "Invalid value '' for 'description' (EntityDescription): length 0 is outside allowed range [1, ]"
    );
}

#[test]
fn schema_validate_wraps_enum_error_with_attribute_name() {
    // End-to-end at the ResourceSchema boundary: the attribute loop in
    // `ResourceSchema::validate` must wrap type errors with the attribute
    // name so every downstream consumer (CLI, LSP) sees the plumbed form.
    let schema = ResourceSchema::new("test.assignment").attribute(
        AttributeSchema::new(
            "target_type",
            AttributeType::enum_(
                crate::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
                Some(vec!["AWS_ACCOUNT".to_string()]),
                vec![],
                None,
                None,
            ),
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
        expected: vec![ExpectedEnumVariant::from_namespaced(
            Some("awscc.sso.Assignment"),
            "TargetType",
            "AWS_ACCOUNT",
            false,
        )],
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
        expected: vec![ExpectedEnumVariant::from_namespaced(
            Some("awscc.sso.Assignment"),
            "TargetType",
            "AWS_ACCOUNT",
            false,
        )],
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
            AttributeType::enum_(
                crate::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
                Some(vec!["AWS_ACCOUNT".to_string()]),
                vec![],
                None,
                None,
            ),
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
        Value::Concrete(ConcreteValue::enum_identifier("aaa".to_string())),
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
            AttributeType::enum_(
                crate::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
                Some(vec!["AWS_ACCOUNT".to_string()]),
                vec![],
                None,
                None,
            ),
        )
        .required(),
    );
    let mut attrs = HashMap::new();
    attrs.insert(
        "target_type".to_string(),
        Value::Concrete(ConcreteValue::enum_identifier("AWS_ACCOUNT".to_string())),
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
            AttributeType::enum_with_base(
                crate::schema::enum_identity("Mode", Some("test.r")),
                AttributeType::string(),
                None,
                vec![],
                Some(legacy_validator(validate_mode)),
                None,
            ),
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
    let t = AttributeType::enum_(
        TypeIdentity::bare("Mode".to_string()),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );
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
    let t = AttributeType::enum_(
        crate::schema::enum_identity("TargetType", Some("awscc.sso.Assignment")),
        Some(vec!["AWS_ACCOUNT".to_string()]),
        vec![],
        None,
        None,
    );
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
fn validate_enum_rejects_double_namespace() {
    let t = AttributeType::enum_(
        crate::schema::enum_identity("InstanceTenancy", Some("awscc.ec2.Vpc")),
        Some(vec![
            "default".to_string(),
            "dedicated".to_string(),
            "host".to_string(),
        ]),
        vec![],
        None,
        None,
    );
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
    let t = AttributeType::float();
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
    let t = AttributeType::float();
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
    let t = AttributeType::int();
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
        .attribute(AttributeSchema::new("name", AttributeType::string()).required())
        .attribute(AttributeSchema::new("count", types::positive_int()))
        .attribute(AttributeSchema::new("enabled", AttributeType::bool()));

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
        .attribute(AttributeSchema::new("name", AttributeType::string()).required());

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
    let t = AttributeType::struct_(
        "Ingress".to_string(),
        vec![
            StructField::new("ip_protocol", AttributeType::string()).required(),
            StructField::new("from_port", AttributeType::int()),
            StructField::new("to_port", AttributeType::int()),
        ],
    );

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
    let t = AttributeType::struct_(
        "Ingress".to_string(),
        vec![
            StructField::new("ip_protocol", AttributeType::string()).required(),
            StructField::new("from_port", AttributeType::int()),
            StructField::new("to_port", AttributeType::int()),
            StructField::new("cidr_ip", AttributeType::string()),
        ],
    );

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
    let t = AttributeType::struct_(
        "Ingress".to_string(),
        vec![
            StructField::new("ip_protocol", AttributeType::string()),
            StructField::new("from_port", AttributeType::int()),
            StructField::new("to_port", AttributeType::int()),
            StructField::new("cidr_ip", AttributeType::string()),
        ],
    );

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
    let t = AttributeType::struct_(
        "SecurityGroupIngress".to_string(),
        vec![
            StructField::new("vpc_id", AttributeType::string()),
            StructField::new("cidr_ip", AttributeType::string()),
        ],
    );

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
    let struct_type = AttributeType::struct_(
        "Ingress".to_string(),
        vec![StructField::new("ip_protocol", AttributeType::string()).required()],
    );
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
    let struct_type = AttributeType::struct_(
        "VersioningConfiguration".to_string(),
        vec![StructField::new("status", AttributeType::string()).required()],
    );

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
    let struct_type = AttributeType::struct_(
        "VersioningConfiguration".to_string(),
        vec![StructField::new("status", AttributeType::string()).required()],
    );

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
        .attribute(AttributeSchema::new("name", AttributeType::string()))
        .attribute(AttributeSchema::new("forbidden", AttributeType::string()))
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
        .attribute(AttributeSchema::new("cidr_block", AttributeType::string()))
        .attribute(AttributeSchema::new(
            "ipv4_ipam_pool_id",
            AttributeType::string(),
        ))
        .attribute(AttributeSchema::new("vpc_id", AttributeType::string()).required())
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
        .attribute(AttributeSchema::new("cidr_block", AttributeType::string()))
        .attribute(AttributeSchema::new(
            "ipv4_ipam_pool_id",
            AttributeType::string(),
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
        .attribute(AttributeSchema::new("a", AttributeType::string()))
        .attribute(AttributeSchema::new("b", AttributeType::string()))
        .attribute(AttributeSchema::new("x", AttributeType::string()))
        .attribute(AttributeSchema::new("y", AttributeType::string()))
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
    let type_a = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("TypeA")),
        None,
        None,
        legacy_validator(|value| {
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
        None,
    );
    let type_b = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("TypeB")),
        None,
        None,
        legacy_validator(|value| {
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
        None,
    );

    let union_type = AttributeType::union(vec![type_a, type_b]);

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
    let principal_type = AttributeType::union(vec![
        AttributeType::struct_(
            "Principal".to_string(),
            vec![
                StructField::new("service", AttributeType::string()),
                StructField::new("federated", AttributeType::string()),
            ],
        ),
        AttributeType::string(),
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
    let type_a = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("TypeA")),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    let type_b = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("TypeB")),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );

    let union_type = AttributeType::union(vec![type_a, type_b]);
    assert_eq!(union_type.type_name(), "TypeA | TypeB");
}

#[test]
fn with_block_name_builder() {
    let attr = AttributeSchema::new("operating_regions", AttributeType::string())
        .with_block_name("operating_region");
    assert_eq!(attr.block_name.as_deref(), Some("operating_region"));
}

#[test]
fn block_name_default_is_none() {
    let attr = AttributeSchema::new("name", AttributeType::string());
    assert!(attr.block_name.is_none());
}

#[test]
fn block_name_map_returns_mapping() {
    let schema = ResourceSchema::new("test.resource")
        .attribute(
            AttributeSchema::new("operating_regions", AttributeType::string())
                .with_block_name("operating_region"),
        )
        .attribute(AttributeSchema::new("name", AttributeType::string()));

    let map = schema.block_name_map();
    assert_eq!(map.len(), 1);
    assert_eq!(map.get("operating_region").unwrap(), "operating_regions");
}

#[test]
fn block_name_map_empty_when_no_block_names() {
    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::new("name", AttributeType::string()));

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
            AttributeSchema::new("operating_regions", AttributeType::string())
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
            .attribute(AttributeSchema::new("name", AttributeType::string())),
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
            AttributeSchema::new("operating_regions", AttributeType::string())
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
        AttributeType::list(AttributeType::struct_("Transition".to_string(), vec![])),
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
            AttributeType::struct_(
                "LifecycleConfiguration".to_string(),
                vec![
                    StructField::new(
                        "transitions",
                        AttributeType::list(AttributeType::struct_(
                            "Transition".to_string(),
                            vec![],
                        )),
                    )
                    .with_block_name("transition"),
                ],
            ),
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
            AttributeType::struct_(
                "LifecycleConfiguration".to_string(),
                vec![
                    StructField::new(
                        "transition",
                        AttributeType::struct_("Transition".to_string(), vec![]),
                    ),
                    StructField::new(
                        "transitions",
                        AttributeType::list(AttributeType::struct_(
                            "Transition".to_string(),
                            vec![],
                        )),
                    )
                    .with_block_name("transition"),
                ],
            ),
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
            AttributeType::struct_(
                "LifecycleConfiguration".to_string(),
                vec![
                    StructField::new(
                        "transition",
                        AttributeType::struct_("Transition".to_string(), vec![]),
                    ),
                    StructField::new(
                        "transitions",
                        AttributeType::list(AttributeType::struct_(
                            "Transition".to_string(),
                            vec![],
                        )),
                    )
                    .with_block_name("transition"),
                ],
            ),
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
                AttributeType::list(AttributeType::struct_(
                    "Ingress".to_string(),
                    vec![StructField::new("ip_protocol", AttributeType::string())],
                )),
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
                AttributeType::list(AttributeType::struct_(
                    "Ingress".to_string(),
                    vec![StructField::new("ip_protocol", AttributeType::string())],
                )),
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
            AttributeType::struct_(
                "Config".to_string(),
                vec![
                    StructField::new(
                        "tag",
                        AttributeType::list(AttributeType::struct_(
                            "Tag".to_string(),
                            vec![
                                StructField::new("key", AttributeType::string()),
                                StructField::new("value", AttributeType::string()),
                            ],
                        )),
                    )
                    .with_block_name("tag"),
                ],
            ),
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
fn resolve_block_names_recurses_through_ref_attribute() {
    // Regression for carina#3349 (awscc s3_bucket/lifecycle): the
    // `lifecycle_configuration` attribute is typed as
    // `AttributeType::ref_("LifecycleConfiguration")`. The
    // `LifecycleConfiguration` def's `rules` field carries
    // `block_name("rule")`. DSL `rule { } rule { }` blocks must be
    // renamed to the canonical `rules` field, but the recursion in
    // `resolve_block_names` previously fell through `_ => {}` for
    // `Ref`, so the rename never visited fields inside the resolved
    // def and the schema later reported `Required attribute 'rules'
    // is missing`.
    let mut inner_map = IndexMap::new();
    // Two `rule { ... }` blocks: parser produces a single List value
    // under the block name `rule`.
    inner_map.insert(
        "rule".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("rule-1".to_string())),
                );
                m
            })),
            Value::Concrete(ConcreteValue::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("rule-2".to_string())),
                );
                m
            })),
        ])),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr(
            "lifecycle_configuration".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        r
    }];

    let lifecycle_def = AttributeType::struct_(
        "LifecycleConfiguration".to_string(),
        vec![
            StructField::new(
                "rules",
                AttributeType::list(AttributeType::struct_(
                    "Rule".to_string(),
                    vec![StructField::new("id", AttributeType::string())],
                )),
            )
            .with_block_name("rule"),
        ],
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new(
                "lifecycle_configuration",
                AttributeType::ref_("LifecycleConfiguration".to_string()),
            ))
            .with_def("LifecycleConfiguration", lifecycle_def),
    );

    resolve_block_names(&mut resources, &schemas).unwrap();

    let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
        _ => panic!("expected Map"),
    };
    assert!(
        lifecycle.contains_key("rules"),
        "expected nested 'rule' block to be renamed to 'rules' through Ref-typed attribute"
    );
    assert!(
        !lifecycle.contains_key("rule"),
        "expected 'rule' key to be removed after rename"
    );
}

#[test]
fn resolve_block_names_recurses_through_ref_inside_struct_field() {
    // Sibling case: a Struct attribute whose nested field is itself
    // `AttributeType::Ref`. The fix must peel `Ref` at the nested
    // recursion in `resolve_block_names_in_map`, not just the
    // top-level attribute walk in `resolve_block_names`.
    let mut inner_map = IndexMap::new();
    let mut lifecycle_inner = IndexMap::new();
    lifecycle_inner.insert(
        "rule".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map({
                let mut m = IndexMap::new();
                m.insert(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("rule-1".to_string())),
                );
                m
            }),
        )])),
    );
    inner_map.insert(
        "lifecycle".to_string(),
        Value::Concrete(ConcreteValue::Map(lifecycle_inner)),
    );

    let mut resources = vec![{
        let mut r = Resource::new("s3.Bucket", "my-bucket");
        r.set_attr(
            "wrapper".to_string(),
            Value::Concrete(ConcreteValue::Map(inner_map)),
        );
        r
    }];

    let lifecycle_def = AttributeType::struct_(
        "LifecycleConfiguration".to_string(),
        vec![
            StructField::new(
                "rules",
                AttributeType::list(AttributeType::struct_(
                    "Rule".to_string(),
                    vec![StructField::new("id", AttributeType::string())],
                )),
            )
            .with_block_name("rule"),
        ],
    );

    let wrapper_type = AttributeType::struct_(
        "Wrapper".to_string(),
        vec![StructField::new(
            "lifecycle",
            AttributeType::ref_("LifecycleConfiguration".to_string()),
        )],
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::new("wrapper", wrapper_type))
            .with_def("LifecycleConfiguration", lifecycle_def),
    );

    resolve_block_names(&mut resources, &schemas).unwrap();

    let wrapper = match resources[0].get_attr("wrapper") {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
        _ => panic!("expected Map"),
    };
    let lifecycle = match wrapper.get("lifecycle") {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
        _ => panic!("expected nested Map under 'lifecycle'"),
    };
    assert!(
        lifecycle.contains_key("rules"),
        "expected nested 'rule' block under Ref-typed field to be renamed to 'rules'"
    );
    assert!(!lifecycle.contains_key("rule"));
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
fn resolved_attr_type_never_returns_ref_after_peel() {
    // Type-safety invariant: `ResolvedAttrType::as_attr` MUST NOT
    // return `AttributeType::Ref`. The newtype's whole purpose is to
    // make this guarantee compiler-checked at every walk-site
    // (carina#3349). This test pins the runtime behavior of the only
    // constructor (`resolve_refs`) against a multi-hop Ref chain and a
    // direct Ref-to-non-Struct shape; if a future change ever lets
    // `Ref` escape, this test catches it before the schema walkers do.
    let mut defs = std::collections::BTreeMap::new();
    defs.insert("Hop1".to_string(), AttributeType::ref_("Hop2".to_string()));
    defs.insert("Hop2".to_string(), AttributeType::ref_("Hop3".to_string()));
    defs.insert("Hop3".to_string(), AttributeType::string());

    let ref_type = AttributeType::ref_("Hop1".to_string());
    let resolved = ref_type.resolve_refs_with_defs(&defs);
    assert!(
        !matches!(resolved.as_attr().kind(), AttrTypeKind::Ref(_)),
        "resolve_refs must never return a Ref after peeling"
    );
    assert!(matches!(
        resolved.as_attr().kind(),
        AttrTypeKind::String { .. }
    ));

    // Non-Ref input is returned as-is (identity behavior).
    let plain = AttributeType::int();
    let resolved = plain.resolve_refs_with_defs(&defs);
    assert!(matches!(
        resolved.as_attr().kind(),
        AttrTypeKind::Int { .. }
    ));
}

#[test]
#[should_panic(expected = "not found in schema defs")]
fn resolved_attr_type_panics_on_dangling_ref() {
    // The other half of the type-safety claim: a dangling `Ref` is a
    // schema-construction bug that must be caught immediately, not
    // silently absorbed. This pins the existing panic behavior so a
    // future refactor cannot accidentally turn it into "return Ref
    // unchanged" (which would re-open the carina#3349 hazard).
    let defs = std::collections::BTreeMap::new();
    let ref_type = AttributeType::ref_("Missing".to_string());
    let _ = ref_type.resolve_refs_with_defs(&defs);
}

#[test]
fn shape_ref_free_returns_err_for_ref_without_panicking() {
    let ref_type = AttributeType::ref_("CaptchaConfig".to_string());
    let err = ref_type
        .shape_ref_free()
        .expect_err("bare Ref projection should return a typed error");

    assert_eq!(err.name, "CaptchaConfig");
    assert_eq!(
        err.to_string(),
        "unresolved AttributeType::Ref(\"CaptchaConfig\") has no defs in scope"
    );
}

#[test]
fn shape_ref_free_projects_non_ref_shape() {
    let attr_type = AttributeType::list(AttributeType::string());

    match attr_type
        .shape_ref_free()
        .expect("non-Ref shape is projectable")
    {
        Shape::List {
            element_type: inner,
            ordered,
            ..
        } => {
            assert!(ordered);
            assert!(matches!(inner.kind(), AttrTypeKind::String { .. }));
        }
        other => panic!("expected list shape, got {other:?}"),
    }
}

#[test]
fn union_members_ref_free_skips_ref_member_and_keeps_sibling() {
    let attr_type = AttributeType::union(vec![
        AttributeType::ref_("ResolvedOnlyWithDefs"),
        AttributeType::string(),
    ]);
    let mut budget = ShapeWalkBudget::new(8);

    let members = attr_type
        .union_members_ref_free_with_budget(&mut budget)
        .expect("the Union itself is Ref-free")
        .expect("the accessor must expose Union members");
    let resolved = members.iter().collect::<Vec<_>>();
    assert_eq!(resolved.len(), 2);
    assert!(
        resolved[0].is_none(),
        "the unresolved Ref member is skipped"
    );
    assert!(matches!(
        resolved[1]
            .expect("String sibling must remain visible")
            .as_attr()
            .kind(),
        AttrTypeKind::String { .. }
    ));
}

#[test]
fn ref_hop_limit_counts_only_the_current_resolution_chain() {
    let prior_names = (0..MAX_REF_PATH_HOPS)
        .map(|index| format!("Prior{index}"))
        .collect::<Vec<_>>();
    let mut visited_refs = prior_names.iter().map(String::as_str).collect::<Vec<_>>();
    let attr_type = AttributeType::ref_("Leaf");
    let defs = std::collections::BTreeMap::from([("Leaf".to_string(), AttributeType::string())]);

    let resolution = resolve_refs_on_path(&attr_type, &defs, &mut visited_refs)
        .expect("an accumulated path must not consume the current chain's hop budget");
    assert!(matches!(
        resolution.attr.kind(),
        AttrTypeKind::String { .. }
    ));
    assert_eq!(visited_refs.len(), MAX_REF_PATH_HOPS + 1);
}

#[test]
fn schema_shape_of_resolves_ref_against_defs() {
    let mut defs = std::collections::BTreeMap::new();
    defs.insert("Alias".to_string(), AttributeType::bool());
    let schema = Schema {
        root: AttributeType::ref_("Alias".to_string()),
        defs,
    };

    assert!(matches!(schema.shape_of(&schema.root), Shape::Bool));
    assert!(matches!(
        schema.resolve_of(&schema.root).as_attr().kind(),
        AttrTypeKind::Bool
    ));
}

#[cfg(test)]
mod projection_api_guard {
    use super::*;

    #[test]
    fn new_projection_apis_are_usable() {
        let attr_type = AttributeType::string();
        assert!(matches!(
            attr_type.shape_ref_free().expect("string is Ref-free"),
            Shape::String { .. }
        ));

        let schema = Schema::flat(AttributeType::int());
        assert!(matches!(schema.shape_of(&schema.root), Shape::Int { .. }));
    }
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
        .attribute(AttributeSchema::new("bucket_name", AttributeType::string()));

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
        .attribute(AttributeSchema::new("bucket_name", AttributeType::string()))
        .attribute(AttributeSchema::new(
            "tags",
            AttributeType::map(AttributeType::string()),
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
fn validate_rejects_read_only_attribute_assignment() {
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("name", AttributeType::string()))
        .attribute(AttributeSchema::new("region", AttributeType::string()).read_only());

    assert!(
        schema.validate(&HashMap::new()).is_ok(),
        "omitting a read-only attribute should remain valid"
    );

    let mut attrs = HashMap::new();
    attrs.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
    );
    assert!(
        schema.validate(&attrs).is_ok(),
        "normal writable attributes should remain valid"
    );

    // Deliberately use the wrong value type as well: writability takes
    // precedence, so this assignment must produce one read-only error rather
    // than a confusing type error (or both errors).
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::Int(42)),
    );
    let errors = schema
        .validate(&attrs)
        .expect_err("assigning a read-only attribute must fail validation");
    assert_eq!(
        errors.len(),
        1,
        "expected one writability error: {errors:?}"
    );
    assert!(
        matches!(&errors[0], TypeError::ReadOnlyAttribute { name } if name == "region"),
        "expected a structured read-only error naming 'region': {errors:?}"
    );
    assert_eq!(
        errors[0].to_string(),
        "Attribute 'region' is populated by the provider and cannot be set"
    );
}

#[test]
fn validate_read_only_block_name_error_uses_user_written_alias() {
    let schema = ResourceSchema::new("test.Resource").attribute(
        AttributeSchema::new(
            "generated_rules",
            AttributeType::list(AttributeType::string()),
        )
        .read_only()
        .with_block_name("generated_rule"),
    );

    let mut attrs = HashMap::new();
    attrs.insert(
        "generated_rule".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("provider-owned".to_string()),
        )])),
    );

    let errors = schema
        .validate(&attrs)
        .expect_err("assigning through a read-only block-name alias must fail validation");
    assert_eq!(
        errors.len(),
        1,
        "expected one writability error: {errors:?}"
    );
    assert!(
        matches!(&errors[0], TypeError::ReadOnlyAttribute { name } if name == "generated_rule"),
        "expected a structured read-only error naming the written alias: {errors:?}"
    );
    assert_eq!(
        errors[0].to_string(),
        "Attribute 'generated_rule' is populated by the provider and cannot be set"
    );
}

#[test]
fn validate_read_only_is_keyed_per_resource_type_not_by_attribute_name() {
    let route_schema = ResourceSchema::new("ec2.Route")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::string()).read_only());
    let vpc_schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::string()));
    let attrs = HashMap::from([(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    )]);

    let route_errors = route_schema
        .validate(&attrs)
        .expect_err("Route.cidr_block must remain provider-populated");
    assert!(
        matches!(&route_errors[..], [TypeError::ReadOnlyAttribute { name }] if name == "cidr_block"),
        "Route must reject its read-only cidr_block: {route_errors:?}"
    );
    assert!(
        vpc_schema.validate(&attrs).is_ok(),
        "Vpc must accept its independently writable cidr_block"
    );
}

#[test]
fn validate_read_only_top_level_name_does_not_reject_writable_nested_struct_field() {
    let origin = AttributeType::struct_(
        "Origin".to_string(),
        vec![
            StructField::new("id", AttributeType::string()).required(),
            StructField::new("domain_name", AttributeType::string()).required(),
        ],
    );
    let schema = ResourceSchema::new("cloudfront.Distribution")
        .attribute(AttributeSchema::new("id", AttributeType::string()).read_only())
        .attribute(AttributeSchema::new("domain_name", AttributeType::string()).read_only())
        .attribute(
            AttributeSchema::new("origins", AttributeType::list(origin)).with_block_name("origin"),
        );

    let mut origin_value = IndexMap::new();
    origin_value.insert(
        "id".to_string(),
        Value::Concrete(ConcreteValue::String("primary-origin".to_string())),
    );
    origin_value.insert(
        "domain_name".to_string(),
        Value::Concrete(ConcreteValue::String("origin.example.com".to_string())),
    );
    let mut attrs = HashMap::from([(
        "origin".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(origin_value),
        )])),
    )]);

    assert!(
        schema.validate(&attrs).is_ok(),
        "nested origin.id and origin.domain_name must remain writable"
    );

    attrs.insert(
        "id".to_string(),
        Value::Concrete(ConcreteValue::String("provider-id".to_string())),
    );
    let errors = schema
        .validate(&attrs)
        .expect_err("top-level Distribution.id must remain provider-populated");
    assert!(
        matches!(&errors[..], [TypeError::ReadOnlyAttribute { name }] if name == "id"),
        "only the top-level id assignment must be rejected: {errors:?}"
    );
}

#[test]
fn validate_unknown_attribute_with_suggestion() {
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::new("bucket_name", AttributeType::string()));

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
            AttributeType::unordered_list(AttributeType::string()),
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
        .attribute(AttributeSchema::new("bucket_name", AttributeType::string()));

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
    match base.raw_shape() {
        RawShape::String { .. } => AttributeType::refined_string_with_validator(
            Some(TypeIdentity::bare(name)),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        ),
        RawShape::Int { .. } => AttributeType::refined_int_with_validator(
            Some(TypeIdentity::bare(name)),
            None,
            crate::schema::legacy_validator(|_| Ok(())),
        ),
        other => panic!("test helper does not support refined base {other:?}"),
    }
}

fn make_custom_anon_pattern(pattern: &str) -> AttributeType {
    AttributeType::refined_string_with_validator(
        None,
        Some(pattern.to_string()),
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    )
}

fn make_custom_anon_len(min: u64, max: u64) -> AttributeType {
    AttributeType::refined_string_with_validator(
        None,
        None,
        Some((Some(min), Some(max))),
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    )
}

fn assignable_with_defs(
    source: &AttributeType,
    source_defs: &std::collections::BTreeMap<String, AttributeType>,
    sink: &AttributeType,
    sink_defs: &std::collections::BTreeMap<String, AttributeType>,
) -> bool {
    source
        .in_schema(source_defs)
        .is_assignable_to(sink.in_schema(sink_defs))
}

fn assignable(source: &AttributeType, sink: &AttributeType) -> bool {
    TypeInSchema::schemaless(source).is_assignable_to(TypeInSchema::schemaless(sink))
}

#[test]
fn assignable_sink_ref_union_matches_inline_union() {
    let source = AttributeType::string();
    let inline = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::list(AttributeType::string()),
    ]);
    let referenced = AttributeType::union(vec![
        AttributeType::ref_("S".to_string()),
        AttributeType::ref_("L".to_string()),
    ]);
    let sink_defs = std::collections::BTreeMap::from([
        ("S".to_string(), AttributeType::string()),
        (
            "L".to_string(),
            AttributeType::list(AttributeType::string()),
        ),
    ]);
    let empty_defs = std::collections::BTreeMap::new();

    assert!(assignable(&source, &inline));
    assert!(assignable_with_defs(
        &source,
        &empty_defs,
        &referenced,
        &sink_defs,
    ));
}

#[test]
fn assignable_source_ref_resolves_against_source_defs() {
    let source = AttributeType::ref_("X".to_string());
    let source_defs =
        std::collections::BTreeMap::from([("X".to_string(), AttributeType::string())]);
    let sink = AttributeType::string();
    let sink_defs = std::collections::BTreeMap::new();

    assert!(assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_ref_chains_resolve_on_both_sides() {
    let source = AttributeType::ref_("SourceX".to_string());
    let source_defs = std::collections::BTreeMap::from([
        (
            "SourceX".to_string(),
            AttributeType::ref_("SourceY".to_string()),
        ),
        ("SourceY".to_string(), AttributeType::string()),
    ]);
    let sink = AttributeType::ref_("SinkX".to_string());
    let sink_defs = std::collections::BTreeMap::from([
        (
            "SinkX".to_string(),
            AttributeType::ref_("SinkY".to_string()),
        ),
        ("SinkY".to_string(), AttributeType::string()),
    ]);

    assert!(assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_missing_ref_defs_reject_without_panicking() {
    let source = AttributeType::ref_("Value".to_string());
    let sink = AttributeType::ref_("Value".to_string());
    let empty_defs = std::collections::BTreeMap::new();
    let string_defs =
        std::collections::BTreeMap::from([("Value".to_string(), AttributeType::string())]);

    assert!(!assignable_with_defs(
        &source,
        &empty_defs,
        &sink,
        &string_defs,
    ));
    assert!(!assignable_with_defs(
        &source,
        &string_defs,
        &sink,
        &empty_defs,
    ));
}

#[test]
fn assignable_source_mutual_ref_cycle_fails_conservatively() {
    let source = AttributeType::ref_("A".to_string());
    let source_defs = std::collections::BTreeMap::from([
        (
            "A".to_string(),
            AttributeType::union(vec![
                AttributeType::ref_("B".to_string()),
                AttributeType::string(),
            ]),
        ),
        (
            "B".to_string(),
            AttributeType::union(vec![
                AttributeType::ref_("A".to_string()),
                AttributeType::int(),
            ]),
        ),
    ]);
    let sink = AttributeType::union(vec![AttributeType::string(), AttributeType::int()]);
    let sink_defs = std::collections::BTreeMap::new();

    // A cyclic source member fails conservatively. Because source-Union
    // assignability requires every member to succeed, the whole source fails.
    assert!(!assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_identity_direction_survives_refs() {
    let generic = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn")),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    let role = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn")),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    let certificate = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::new(
            Some("aws"),
            ["acm", "Certificate"],
            "Arn",
        )),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    let source = AttributeType::ref_("Value".to_string());
    let sink = AttributeType::ref_("Value".to_string());
    let generic_defs = std::collections::BTreeMap::from([("Value".to_string(), generic)]);
    let role_defs = std::collections::BTreeMap::from([("Value".to_string(), role)]);
    let certificate_defs = std::collections::BTreeMap::from([("Value".to_string(), certificate)]);

    assert!(assignable_with_defs(
        &source,
        &role_defs,
        &sink,
        &generic_defs,
    ));
    assert!(!assignable_with_defs(
        &source,
        &generic_defs,
        &sink,
        &role_defs,
    ));
    assert!(!assignable_with_defs(
        &source,
        &role_defs,
        &sink,
        &certificate_defs,
    ));
}

#[test]
fn assignable_list_ref_name_collision_uses_each_schema_defs() {
    let source = AttributeType::list(AttributeType::ref_("Tag".to_string()));
    let source_defs = std::collections::BTreeMap::from([(
        "Tag".to_string(),
        AttributeType::struct_(
            "Tag".to_string(),
            vec![StructField::new("key", AttributeType::string())],
        ),
    )]);
    let sink = AttributeType::list(AttributeType::ref_("Tag".to_string()));
    let sink_defs =
        std::collections::BTreeMap::from([("Tag".to_string(), AttributeType::string())]);

    assert!(!assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_list_nested_ref_matches_inline_element() {
    let source = AttributeType::list(AttributeType::ref_("S".to_string()));
    let source_defs =
        std::collections::BTreeMap::from([("S".to_string(), AttributeType::string())]);
    let sink = AttributeType::list(AttributeType::string());
    let sink_defs = std::collections::BTreeMap::new();

    assert!(assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_inline_lists_remain_assignable() {
    assert!(assignable(
        &AttributeType::list(AttributeType::string()),
        &AttributeType::list(AttributeType::string()),
    ));
}

#[test]
fn assignable_list_ignores_order_and_length_metadata() {
    let source = AttributeType::refined_list(
        AttributeType::string(),
        false,
        Some((Some(1), Some(2))),
        crate::schema::legacy_validator(|_| Ok(())),
    );
    let sink = AttributeType::refined_list(
        AttributeType::string(),
        true,
        Some((Some(10), Some(20))),
        crate::schema::legacy_validator(|_| Ok(())),
    );

    assert!(assignable(&source, &sink));
}

#[test]
fn assignable_cyclic_list_ref_terminates_conservatively() {
    let source = AttributeType::ref_("A".to_string());
    let source_defs = std::collections::BTreeMap::from([(
        "A".to_string(),
        AttributeType::list(AttributeType::ref_("A".to_string())),
    )]);
    let sink = AttributeType::list(AttributeType::string());
    let sink_defs = std::collections::BTreeMap::new();

    assert!(!assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_map_nested_ref_matches_inline_value() {
    let source = AttributeType::map(AttributeType::ref_("S".to_string()));
    let source_defs =
        std::collections::BTreeMap::from([("S".to_string(), AttributeType::string())]);
    let sink = AttributeType::map(AttributeType::string());
    let sink_defs = std::collections::BTreeMap::new();

    assert!(assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_map_ref_name_collision_rejects_incompatible_value() {
    let source = AttributeType::map(AttributeType::ref_("Tag".to_string()));
    let source_defs = std::collections::BTreeMap::from([(
        "Tag".to_string(),
        AttributeType::struct_(
            "Tag".to_string(),
            vec![StructField::new("key", AttributeType::string())],
        ),
    )]);
    let sink = AttributeType::map(AttributeType::ref_("Tag".to_string()));
    let sink_defs =
        std::collections::BTreeMap::from([("Tag".to_string(), AttributeType::string())]);

    assert!(!assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_map_key_refs_use_each_schema_defs() {
    let source = AttributeType::map_with_key(
        AttributeType::ref_("Key".to_string()),
        AttributeType::string(),
    );
    let source_defs = std::collections::BTreeMap::from([("Key".to_string(), AttributeType::int())]);
    let sink = AttributeType::map_with_key(
        AttributeType::ref_("Key".to_string()),
        AttributeType::string(),
    );
    let sink_defs =
        std::collections::BTreeMap::from([("Key".to_string(), AttributeType::string())]);

    assert!(!assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_identical_inline_unions_are_assignable() {
    let source = AttributeType::union(vec![AttributeType::string(), AttributeType::int()]);
    let sink = AttributeType::union(vec![AttributeType::string(), AttributeType::int()]);

    assert!(assignable(&source, &sink));
}

#[test]
fn assignable_identical_ref_membered_unions_are_assignable() {
    let source = AttributeType::union(vec![
        AttributeType::ref_("S".to_string()),
        AttributeType::ref_("L".to_string()),
    ]);
    let source_defs = std::collections::BTreeMap::from([
        ("S".to_string(), AttributeType::string()),
        (
            "L".to_string(),
            AttributeType::list(AttributeType::string()),
        ),
    ]);
    let sink = AttributeType::union(vec![
        AttributeType::ref_("S".to_string()),
        AttributeType::ref_("L".to_string()),
    ]);
    let sink_defs = source_defs.clone();

    assert!(assignable_with_defs(
        &source,
        &source_defs,
        &sink,
        &sink_defs,
    ));
}

#[test]
fn assignable_union_non_subset_is_rejected() {
    let source = AttributeType::union(vec![AttributeType::string(), AttributeType::bool()]);
    let sink = AttributeType::union(vec![AttributeType::string(), AttributeType::int()]);

    assert!(!assignable(&source, &sink));
}

#[test]
fn assignable_single_union_side_semantics_are_unchanged() {
    let string_or_int = AttributeType::union(vec![AttributeType::string(), AttributeType::int()]);
    assert!(assignable(&AttributeType::string(), &string_or_int));

    let strings = AttributeType::union(vec![AttributeType::string(), AttributeType::string()]);
    assert!(assignable(&strings, &AttributeType::string()));
    assert!(!assignable(&string_or_int, &AttributeType::string()));
}

#[test]
fn assignable_empty_source_union_is_vacuously_assignable() {
    let bottom = AttributeType::union(vec![]);

    // An empty union has no possible values, so it is the bottom type and is
    // assignable to every sink under the source-subset rule.
    assert!(assignable(&bottom, &AttributeType::bool()));
}

#[test]
fn assignable_list_identified_string_flows_to_anonymous_string() {
    let source = AttributeType::list(make_custom("VpcId", AttributeType::string()));
    let sink = AttributeType::list(AttributeType::string());

    // Structural List comparison intentionally changes the old name-only
    // rejection: element assignability now mirrors VpcId -> String scalars.
    assert!(assignable(&source, &sink));
}

#[test]
fn assignable_list_int_range_rejects_wider_source() {
    let source = AttributeType::list(AttributeType::refined_int_with_validator(
        None,
        Some((Some(0), Some(100))),
        crate::schema::legacy_validator(|_| Ok(())),
    ));
    let sink = AttributeType::list(AttributeType::refined_int_with_validator(
        None,
        Some((Some(0), Some(10))),
        crate::schema::legacy_validator(|_| Ok(())),
    ));

    // Structural List comparison intentionally closes the old name-only
    // false acceptance: the source element range is not contained by sink.
    assert!(!assignable(&source, &sink));
}

#[test]
fn resource_schema_type_in_schema_uses_its_own_defs() {
    let schema = ResourceSchema::new("test.Item")
        .attribute(AttributeSchema::new(
            "value",
            AttributeType::ref_("Value".to_string()),
        ))
        .with_def("Value", AttributeType::string());
    let attr = &schema.attributes["value"].attr_type;

    assert_eq!(schema.type_in_schema(attr).resolved_type_name(), "String");
}

#[test]
fn type_in_schema_display_name_falls_back_for_missing_ref() {
    let attr = AttributeType::ref_("Missing".to_string());

    assert_eq!(
        attr.in_schema(crate::schema::empty_defs_for_schema_walks())
            .resolved_type_name(),
        "Ref(Missing)",
    );
}

#[test]
fn type_in_schema_display_name_resolves_refs_in_composite_types() {
    let attr = AttributeType::union(vec![
        AttributeType::list(AttributeType::ref_("S".to_string())),
        AttributeType::map(AttributeType::ref_("I".to_string())),
    ]);
    let defs = std::collections::BTreeMap::from([
        ("S".to_string(), AttributeType::string()),
        ("I".to_string(), AttributeType::int()),
    ]);

    assert_eq!(
        attr.in_schema(&defs).resolved_type_name(),
        "List<String> | Map<Int>",
    );
}

#[test]
fn type_in_schema_display_name_falls_back_at_cyclic_node() {
    let attr = AttributeType::ref_("A".to_string());
    let defs = std::collections::BTreeMap::from([(
        "A".to_string(),
        AttributeType::list(AttributeType::ref_("A".to_string())),
    )]);

    assert_eq!(attr.in_schema(&defs).resolved_type_name(), "List<Ref(A)>",);
}

#[test]
fn assignable_allows_same_primitives() {
    assert!(assignable(
        &AttributeType::string(),
        &AttributeType::string()
    ));
    assert!(assignable(&AttributeType::int(), &AttributeType::int()));
    assert!(!assignable(&AttributeType::int(), &AttributeType::string()));
    assert!(!assignable(&AttributeType::bool(), &AttributeType::int()));
}

#[test]
fn assignable_rejects_distinct_semantic_names() {
    let vpc = make_custom("VpcId", AttributeType::string());
    let subnet = make_custom("SubnetId", AttributeType::string());
    assert!(!assignable(&vpc, &subnet));
    assert!(!assignable(&subnet, &vpc));
}

#[test]
fn assignable_allows_same_semantic_name() {
    let a = make_custom("VpcId", AttributeType::string());
    let b = make_custom("VpcId", AttributeType::string());
    assert!(assignable(&a, &b));
}

/// carina#2807: two providers exposing a same-named custom type
/// (`aws.Region` vs `gcp.Region`, different formats) must be distinct
/// types. Before the structured `TypeIdentity`, a flat `semantic_name`
/// string made them collide — `is_assignable_to` would treat them as
/// the same type because the strings were equal.
#[test]
fn assignable_rejects_same_kind_across_providers() {
    let provider_custom = |provider: &str| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(
                Some(provider),
                Vec::<String>::new(),
                "Region",
            )),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };
    let aws_region = provider_custom("aws");
    let gcp_region = provider_custom("gcp");
    assert!(!assignable(&aws_region, &gcp_region));
    assert!(!assignable(&gcp_region, &aws_region));
}

#[test]
fn assignable_rejects_same_enum_kind_across_namespaces() {
    let provider_enum = |namespace: &str| {
        AttributeType::enum_(
            crate::schema::enum_identity("Mode", Some(namespace)),
            Some(vec!["fast".to_string()]),
            vec![],
            None,
            None,
        )
    };
    let aws_enum = provider_enum("aws.foo");
    let bar_enum = provider_enum("aws.bar");
    assert!(!assignable(&aws_enum, &bar_enum));
    assert!(!assignable(&bar_enum, &aws_enum));
}

/// carina#3413: same `TypeIdentity` for an Enum constructed independently
/// must remain mutually assignable. Mirrors the Custom-variant tests above
/// in the Enum codepath (carina#3412's identity-aware arm).
#[test]
fn assignable_accepts_same_enum_typeidentity_from_distinct_constructions() {
    let mk = || {
        AttributeType::enum_(
            crate::schema::enum_identity("Status", Some("aws.s3.Bucket.Versioning")),
            Some(vec!["enabled".to_string(), "suspended".to_string()]),
            vec![],
            None,
            None,
        )
    };
    let a = mk();
    let b = mk();
    assert!(assignable(&a, &b));
    assert!(assignable(&b, &a));
}

/// The generic provider-scoped `aws.Arn` (no service/resource segments)
/// stays assignable against a more specific `aws.iam.Role.Arn`: an
/// `segments` is **directional**: an empty `segments` source carries
/// no evidence of satisfying a populated `segments` sink. Closes
/// carina#3218.
#[test]
fn assignable_specific_arn_flows_into_generic_arn() {
    let mk = |segments: &[&str], pattern: Option<&str>| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(Some("aws"), segments.to_vec(), "Arn")),
            pattern.map(str::to_string),
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };
    let generic = mk(&[], None);
    let role_arn = mk(&["iam", "Role"], None);

    // Specific → generic: narrower source satisfies wider sink.
    assert!(assignable(&role_arn, &generic));

    // Generic → specific: the empty-segments source carries no
    // evidence of being an IAM Role ARN. Must be rejected.
    assert!(!assignable(&generic, &role_arn));

    // …and two specific ARNs with different service/resource differ.
    let cert_arn = mk(&["acm", "Certificate"], None);
    assert!(!assignable(&role_arn, &cert_arn));
    assert!(!assignable(&cert_arn, &role_arn));

    // Matching patterns do not make different same-depth identities compatible.
    let s3_bucket_arn = mk(
        &["s3", "Bucket"],
        Some("^arn:(aws|aws-cn|aws-us-gov):s3:::.+$"),
    );
    let s3_object_arn = mk(
        &["s3", "Object"],
        Some("^arn:(aws|aws-cn|aws-us-gov):s3:::.+$"),
    );
    assert!(!assignable(&s3_bucket_arn, &s3_object_arn));
    assert!(!assignable(&s3_object_arn, &s3_bucket_arn));
}

#[test]
fn assignable_specific_arn_with_pattern_flows_into_generic_arn_with_pattern() {
    let mk = |segments: &[&str], pattern: &str| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(Some("aws"), segments.to_vec(), "Arn")),
            Some(pattern.to_string()),
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };

    let generic = mk(&[], "^arn:(aws|aws-cn|aws-us-gov):[^:]+:.+$");
    let bucket_arn = mk(&["s3", "Bucket"], "^arn:(aws|aws-cn|aws-us-gov):s3:::.+$");

    assert!(assignable(&bucket_arn, &generic));
    assert!(!assignable(&generic, &bucket_arn));
}

#[test]
fn assignable_specific_arn_without_pattern_flows_into_generic_arn_with_pattern() {
    let mk = |segments: &[&str], pattern: Option<&str>| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(Some("aws"), segments.to_vec(), "Arn")),
            pattern.map(str::to_string),
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };

    let generic = mk(&[], Some("^arn:(aws|aws-cn|aws-us-gov):[^:]+:.+$"));
    let role_arn = mk(&["iam", "Role"], None);

    assert!(assignable(&role_arn, &generic));
}

#[test]
fn assignable_identified_custom_length_must_be_contained() {
    let mk = |length: Option<(Option<u64>, Option<u64>)>| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::bare("SizedString")),
            None,
            length,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };

    let narrow = mk(Some((Some(1), Some(32))));
    let wide = mk(Some((Some(1), Some(64))));
    assert!(assignable(&narrow, &wide));
    assert!(!assignable(&wide, &narrow));

    let unproven = mk(None);
    assert!(!assignable(&unproven, &wide));
}

/// carina#3413: two callers that independently construct the SAME
/// `TypeIdentity` (e.g. both call `provider_type("ec2", "Vpc", "Id")`,
/// producing `aws.ec2.Vpc.Id`) must produce AttributeType values that
/// are mutually assignable. This is the type-safety property that
/// lets a value flow from one provider's resource attribute into
/// another provider's resource attribute without a runtime alias
/// check, after the awscc-local carina-aws-types crate was dismantled
/// in carina-provider-awscc#338.
#[test]
fn assignable_accepts_same_typeidentity_from_distinct_constructions_vpc_id() {
    let mk = || {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(Some("aws"), vec!["ec2", "Vpc"], "Id")),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };
    let a = mk();
    let b = mk();
    assert!(assignable(&a, &b));
    assert!(assignable(&b, &a));
}

/// carina#3413: same as above but for the `aws.AccountId` form which
/// has empty `segments`. Guards against accidental loosening that
/// would only cover populated-segments identities.
#[test]
fn assignable_accepts_same_typeidentity_from_distinct_constructions_account_id() {
    let mk = || {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(
                Some("aws"),
                Vec::<String>::new(),
                "AccountId",
            )),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };
    let a = mk();
    let b = mk();
    assert!(assignable(&a, &b));
    assert!(assignable(&b, &a));
}

/// carina#3413: deeper segments path (`aws.iam.Role.Arn`). One of the
/// concrete identities motivating the original issue body.
#[test]
fn assignable_accepts_same_typeidentity_from_distinct_constructions_iam_role_arn() {
    let mk = || {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(Some("aws"), vec!["iam", "Role"], "Arn")),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };
    let a = mk();
    let b = mk();
    assert!(assignable(&a, &b));
    assert!(assignable(&b, &a));
}

/// carina#3413 guard: canonicalizing AWS identities to `aws.*` must
/// NOT loosen `is_assignable_to` between genuinely different
/// providers. A hypothetical `gcp.AccountId` must still be rejected
/// against `aws.AccountId`. Complements
/// `assignable_rejects_same_kind_across_providers` in the AccountId
/// context that motivated carina#3413.
#[test]
fn assignable_rejects_account_id_across_genuinely_different_providers() {
    let mk = |provider: &str| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(
                Some(provider),
                Vec::<String>::new(),
                "AccountId",
            )),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };
    let aws_account = mk("aws");
    let gcp_account = mk("gcp");
    assert!(!assignable(&aws_account, &gcp_account));
    assert!(!assignable(&gcp_account, &aws_account));
}

/// Directional per-axis subsumption: source missing a populated sink
/// axis (provider or segments) is rejected; sink missing a populated
/// source axis is accepted (widening).
#[test]
fn assignable_identity_axis_directionality() {
    let mk = |provider: Option<&str>, segments: &[&str]| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::new(provider, segments.to_vec(), "Arn")),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        )
    };

    // provider: None source → Some sink rejected
    let bare = mk(None, &[]);
    let scoped = mk(Some("aws"), &[]);
    assert!(!assignable(&bare, &scoped));
    // The reverse (sink None) is the widening case and is accepted.
    assert!(assignable(&scoped, &bare));

    // segments: [] source → ["iam", "Role"] sink rejected
    let generic = mk(Some("aws"), &[]);
    let role = mk(Some("aws"), &["iam", "Role"]);
    assert!(!assignable(&generic, &role));
    assert!(assignable(&role, &generic));
}

#[test]
fn assignable_narrow_to_anonymous_unconstrained_sink() {
    // Semantic source with no pattern assigns to fully-anonymous unconstrained sink.
    let account = make_custom("AwsAccountId", AttributeType::string());
    let anon = AttributeType::refined_string_with_validator(
        None,
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert!(assignable(&account, &anon));
}

#[test]
fn assignable_source_without_pattern_rejected_by_patterned_sink() {
    let account = make_custom("AwsAccountId", AttributeType::string());
    let anon = make_custom_anon_pattern("^\\d{12}$");
    // Source has no pattern; sink demands one → NG.
    assert!(!assignable(&account, &anon));
}

#[test]
fn assignable_anon_to_anon_length_containment() {
    let narrow = make_custom_anon_len(1, 36);
    let wide = make_custom_anon_len(1, 64);
    assert!(assignable(&narrow, &wide));
    assert!(!assignable(&wide, &narrow));
}

#[test]
fn assignable_rejects_non_custom_to_custom() {
    let vpc = make_custom("VpcId", AttributeType::string());
    assert!(!assignable(&AttributeType::string(), &vpc));
}

#[test]
fn assignable_custom_to_non_custom_recurses_on_base() {
    // AwsAccountId (base: String) assigns to a plain String sink.
    let account = make_custom("AwsAccountId", AttributeType::string());
    assert!(assignable(&account, &AttributeType::string()));
}

#[test]
fn assignable_union_sink_accepts_assignable_member() {
    let vpc = make_custom("VpcId", AttributeType::string());
    let other_vpc = make_custom("VpcId", AttributeType::string());
    let union = AttributeType::union(vec![vpc, AttributeType::string()]);
    assert!(assignable(&other_vpc, &union));
}

#[test]
fn assignable_union_source_requires_all_members_assignable() {
    // All members of source must be assignable to sink.
    let vpc = make_custom("VpcId", AttributeType::string());
    let anon_any = AttributeType::refined_string_with_validator(
        None,
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    let both_ok = AttributeType::union(vec![vpc.clone(), vpc.clone()]);
    assert!(assignable(&both_ok, &vpc));

    let subnet = make_custom("SubnetId", AttributeType::string());
    let mixed = AttributeType::union(vec![vpc.clone(), subnet]);
    // One member (SubnetId) not assignable to VpcId sink → whole union NG.
    assert!(!assignable(&mixed, &vpc));

    // But is assignable to an anonymous unconstrained sink.
    assert!(assignable(&mixed, &anon_any));
}

#[test]
fn semantic_custom_assigns_to_anonymous_unconstrained_sink() {
    // Replaces the old buggy `is_compatible_with_two_string_based_customs`
    // which asserted VpcId <-> SubnetId were symmetric-compatible.
    let vpc = make_custom("VpcId", AttributeType::string());
    let anon = AttributeType::refined_string_with_validator(
        None,
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert!(assignable(&vpc, &anon));
    // Reverse: anon has no proof it's a VpcId → NG.
    assert!(!assignable(&anon, &vpc));
}

#[test]
fn assignable_int_custom_rejects_string_custom() {
    let int_custom = make_custom("Port", AttributeType::int());
    let string_custom = make_custom("VpcId", AttributeType::string());
    assert!(!assignable(&int_custom, &string_custom));
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
    AttributeType::refined_string_with_validator(
        None,
        pattern.map(str::to_string),
        length,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    )
}

#[test]
fn assignable_anon_pattern_equal_strings_compatible() {
    let a = make_custom_anon_pattern_and_len(Some("^a+$"), None);
    let b = make_custom_anon_pattern_and_len(Some("^a+$"), None);
    assert!(assignable(&a, &b));
}

#[test]
fn assignable_anon_pattern_differing_strings_incompatible() {
    // Even if both regexes might describe overlapping languages, the
    // implementation does not prove containment — differing pattern
    // strings are conservatively rejected.
    let a = make_custom_anon_pattern_and_len(Some("^a+$"), None);
    let b = make_custom_anon_pattern_and_len(Some("^a*$"), None);
    assert!(!assignable(&a, &b));
    assert!(!assignable(&b, &a));
}

#[test]
fn assignable_anon_pattern_source_none_sink_some_rejected() {
    // Source has no pattern; sink demands one — source has no proof
    // its values match the sink's pattern.
    let source = make_custom_anon_pattern_and_len(None, None);
    let sink = make_custom_anon_pattern_and_len(Some("^x+$"), None);
    assert!(!assignable(&source, &sink));
}

#[test]
fn assignable_anon_pattern_source_some_sink_none_compatible() {
    // Sink has no pattern constraint — any source pattern is fine.
    let source = make_custom_anon_pattern_and_len(Some("^x+$"), None);
    let sink = make_custom_anon_pattern_and_len(None, None);
    assert!(assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_source_narrower_compatible() {
    // Source length range ⊂ sink range → compatible.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(20), Some(30))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_source_wider_min_rejected() {
    // Source min < sink min → values shorter than sink allows could leak through.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(5), Some(40))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_source_wider_max_rejected() {
    // Source max > sink max → values longer than sink allows could leak through.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(50))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_source_unbounded_max_against_bounded_sink_rejected() {
    // Source has no upper bound; sink does → source could exceed sink max.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(10), None)));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_source_unbounded_min_against_bounded_sink_rejected() {
    // Source has no lower bound; sink does → source could fall below sink min.
    let source = make_custom_anon_pattern_and_len(None, Some((None, Some(40))));
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_source_none_sink_some_rejected() {
    // Source has no length constraint at all; sink does → no proof.
    let source = make_custom_anon_pattern_and_len(None, None);
    let sink = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    assert!(!assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_source_some_sink_none_compatible() {
    // Sink has no length constraint — any source length is fine.
    let source = make_custom_anon_pattern_and_len(None, Some((Some(10), Some(40))));
    let sink = make_custom_anon_pattern_and_len(None, None);
    assert!(assignable(&source, &sink));
}

#[test]
fn assignable_anon_length_both_none_compatible() {
    let source = make_custom_anon_pattern_and_len(None, None);
    let sink = make_custom_anon_pattern_and_len(None, None);
    assert!(assignable(&source, &sink));
}

#[test]
fn custom_carries_semantic_name_pattern_length() {
    let t = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("VpcId")),
        Some("^vpc-[a-f0-9]+$".to_string()),
        Some((Some(8), Some(21))),
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    match t.kind() {
        AttrTypeKind::String {
            identity,
            pattern,
            length,
            ..
        } => {
            assert_eq!(identity.as_ref().map(|id| id.kind.as_str()), Some("VpcId"));
            assert_eq!(pattern.as_deref(), Some("^vpc-[a-f0-9]+$"));
            assert_eq!(*length, Some((Some(8), Some(21))));
        }
        _ => panic!("expected refined String"),
    }
}

#[test]
fn custom_pattern_rejects_and_accepts_string_values() {
    let attr = AttributeType::refined_string_with_validator(
        None,
        Some("^[a-z]+$".to_string()),
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );

    let err = attr
        .validate(&Value::Concrete(ConcreteValue::String("ABC".to_string())))
        .unwrap_err();
    assert_eq!(
        err,
        TypeError::PatternMismatch {
            value: "ABC".to_string(),
            pattern: "^[a-z]+$".to_string(),
            attribute: None,
            type_name: None,
        }
    );
    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::String("abc".to_string())))
            .is_ok()
    );
}

#[test]
fn custom_pattern_and_length_passes_still_run_validator() {
    let attr = AttributeType::refined_string_with_validator(
        None,
        Some("^[a-z]+$".to_string()),
        Some((Some(1), Some(10))),
        validator(|_| {
            Err(TypeError::ValidationFailed {
                message: "closure rejected".to_string(),
            })
        }),
        None,
    );

    let err = attr
        .validate(&Value::Concrete(ConcreteValue::String("abc".to_string())))
        .unwrap_err();
    assert_eq!(
        err,
        TypeError::ValidationFailed {
            message: "closure rejected".to_string(),
        }
    );
}

#[test]
fn custom_pattern_rejects_wafv2_description_parentheses() {
    let pattern = r"^[a-zA-Z0-9=:#@/\-,.][a-zA-Z0-9+=:#@/\-,.\s]+[a-zA-Z0-9+=:#@/\-,.]{1,256}$";
    let attr = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("EntityDescription")),
        Some(pattern.to_string()),
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );

    let err = attr
        .validate(&Value::Concrete(ConcreteValue::String(
            "Protects the Carina Provider Registry (dev) CloudFront distribution".to_string(),
        )))
        .unwrap_err();
    assert_eq!(
        err,
        TypeError::PatternMismatch {
            value: "Protects the Carina Provider Registry (dev) CloudFront distribution"
                .to_string(),
            pattern: pattern.to_string(),
            attribute: None,
            type_name: Some("EntityDescription".to_string()),
        }
    );
    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::String(
            "Protects the registry dev distribution".to_string()
        )))
        .is_ok()
    );
}

#[test]
fn custom_length_uses_character_count_not_bytes() {
    let attr = AttributeType::refined_string_with_validator(
        None,
        None,
        Some((Some(1), Some(5))),
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );

    let err = attr
        .validate(&Value::Concrete(ConcreteValue::String(
            "abcdef".to_string(),
        )))
        .unwrap_err();
    assert_eq!(
        err,
        TypeError::LengthOutOfRange {
            value: "abcdef".to_string(),
            length: 6,
            min: Some(1),
            max: Some(5),
            attribute: None,
            type_name: None,
        }
    );
    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::String("abcde".to_string())))
            .is_ok()
    );
    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::String(
            "ねこに".to_string()
        )))
        .is_ok()
    );
}

#[test]
fn custom_length_enforces_minimum_bound() {
    let attr = AttributeType::refined_string_with_validator(
        None,
        None,
        Some((Some(2), None)),
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );

    let err = attr
        .validate(&Value::Concrete(ConcreteValue::String("a".to_string())))
        .unwrap_err();
    assert_eq!(
        err,
        TypeError::LengthOutOfRange {
            value: "a".to_string(),
            length: 1,
            min: Some(2),
            max: None,
            attribute: None,
            type_name: None,
        }
    );
    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::String("ab".to_string())))
            .is_ok()
    );
}

#[test]
fn custom_int_base_maps_length_to_range() {
    let attr = AttributeType::refined_int_with_validator(
        None,
        Some((Some(100), Some(200)))
            .map(|(min, max)| (min.map(|v| v as i64), max.map(|v| v as i64))),
        crate::schema::legacy_validator(|_| Ok(())),
    );

    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::Int(150)))
            .is_ok()
    );
    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::Int(5)))
            .is_err()
    );
}

#[test]
fn custom_uncompilable_pattern_is_non_fatal() {
    let pattern = r"(?=x)";
    assert!(
        regex::Regex::new(pattern).is_err(),
        "test must use a pattern unsupported by the regex crate"
    );
    let attr = AttributeType::refined_string_with_validator(
        None,
        Some(pattern.to_string()),
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );

    assert!(
        attr.validate(&Value::Concrete(ConcreteValue::String(
            "anything".to_string()
        )))
        .is_ok()
    );
}

#[test]
fn schema_validate_attr_dispatches_to_custom_pattern_validation() {
    let attr = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("Slug")),
        Some("^[a-z]+$".to_string()),
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    let schema = Schema::flat(AttributeType::string());

    let err = schema
        .validate_attr(
            &attr,
            &Value::Concrete(ConcreteValue::String("ABC".to_string())),
        )
        .unwrap_err();
    assert_eq!(
        err,
        TypeError::PatternMismatch {
            value: "ABC".to_string(),
            pattern: "^[a-z]+$".to_string(),
            attribute: None,
            type_name: Some("Slug".to_string()),
        }
    );
}

#[test]
fn custom_type_name_anonymous_pattern_only() {
    let t = AttributeType::refined_string_with_validator(
        None,
        Some("^foo$".to_string()),
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert_eq!(t.type_name(), "String(pattern)");
}

#[test]
fn custom_type_name_anonymous_length_only() {
    let t = AttributeType::refined_string_with_validator(
        None,
        None,
        Some((Some(1), Some(64))),
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert_eq!(t.type_name(), "String(len: 1..=64)");
}

#[test]
fn custom_type_name_anonymous_pattern_and_length() {
    let t = AttributeType::refined_string_with_validator(
        None,
        Some("^.*$".to_string()),
        Some((Some(1), Some(64))),
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
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

    // Type identity: refined String with kind "Email"
    match t.kind() {
        AttrTypeKind::String { identity, .. } => {
            assert_eq!(identity.as_ref().map(|id| id.kind.as_str()), Some("Email"));
        }
        other => panic!("Expected refined String, got: {:?}", other),
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

#[test]
fn validate_http_response_status_code_function_directly() {
    // Valid: 2XX/4XX/5XX
    assert!(validate_http_response_status_code("200").is_ok());
    assert!(validate_http_response_status_code("299").is_ok());
    assert!(validate_http_response_status_code("400").is_ok());
    assert!(validate_http_response_status_code("499").is_ok());
    assert!(validate_http_response_status_code("500").is_ok());
    assert!(validate_http_response_status_code("599").is_ok());

    // Invalid: disallowed leading digit (fence-post boundaries pin the
    // excluded 1XX/3XX/6XX ranges explicitly).
    assert!(validate_http_response_status_code("100").is_err());
    assert!(validate_http_response_status_code("199").is_err());
    assert!(validate_http_response_status_code("300").is_err());
    assert!(validate_http_response_status_code("301").is_err());
    assert!(validate_http_response_status_code("399").is_err());
    assert!(validate_http_response_status_code("600").is_err());
    // Invalid: wrong length
    assert!(validate_http_response_status_code("20").is_err());
    assert!(validate_http_response_status_code("2000").is_err());
    assert!(validate_http_response_status_code("").is_err());
    // Invalid: non-digit
    assert!(validate_http_response_status_code("nonsense").is_err());
    assert!(validate_http_response_status_code("12a").is_err());
}

#[test]
fn validate_redirect_protocol_function_directly() {
    assert!(validate_redirect_protocol("HTTP").is_ok());
    assert!(validate_redirect_protocol("HTTPS").is_ok());
    assert!(validate_redirect_protocol("#{protocol}").is_ok());

    assert!(validate_redirect_protocol("").is_err());
    assert!(validate_redirect_protocol("http").is_err());
    assert!(validate_redirect_protocol("#{host}").is_err());
    assert!(validate_redirect_protocol("#{port}").is_err());
    assert!(validate_redirect_protocol("#{path}").is_err());
    assert!(validate_redirect_protocol("#{query}").is_err());
}

#[test]
fn validate_redirect_host_function_directly() {
    assert!(validate_redirect_host("example").is_ok());
    assert!(validate_redirect_host("bad.example.com").is_ok());
    assert!(validate_redirect_host("www-#{host}").is_ok());
    assert!(validate_redirect_host("*-api?").is_ok());
    assert!(validate_redirect_host("https://example.com:443").is_ok());

    assert!(validate_redirect_host("").is_err());
    assert!(validate_redirect_host(&"a".repeat(128)).is_ok());
    assert!(validate_redirect_host(&"a".repeat(129)).is_err());
    assert!(validate_redirect_host("#{protocol}").is_err());
    assert!(validate_redirect_host("#{path}").is_err());
}

#[test]
fn validate_redirect_port_function_directly() {
    assert!(validate_redirect_port("1").is_ok());
    assert!(validate_redirect_port("443").is_ok());
    assert!(validate_redirect_port("65535").is_ok());
    assert!(validate_redirect_port("#{port}").is_ok());

    assert!(validate_redirect_port("").is_err());
    assert!(validate_redirect_port("0").is_err());
    assert!(validate_redirect_port("65536").is_err());
    // Non-decimal forms must be rejected even though u32::from_str accepts them.
    assert!(validate_redirect_port("+443").is_err());
    assert!(validate_redirect_port("0443").is_err());
    assert!(validate_redirect_port("#{path}").is_err());
    assert!(validate_redirect_port("443?").is_err());
}

#[test]
fn validate_redirect_path_function_directly() {
    assert!(validate_redirect_path("/").is_ok());
    assert!(validate_redirect_path("/#{host}:#{port}/#{path}").is_ok());
    assert!(validate_redirect_path("/a*b?&-._$~\"'@:+/").is_ok());
    assert!(validate_redirect_path("/encoded/%2F?x=1").is_ok());
    assert!(validate_redirect_path("/bad#path").is_ok());

    assert!(validate_redirect_path("").is_err());
    assert!(validate_redirect_path(&format!("/{}", "a".repeat(127))).is_ok());
    assert!(validate_redirect_path(&format!("/{}", "a".repeat(128))).is_err());
    assert!(validate_redirect_path("#{path}").is_err());
    assert!(validate_redirect_path("/#{protocol}").is_err());
    assert!(validate_redirect_path("/#{query}").is_err());
}

#[test]
fn validate_redirect_query_function_directly() {
    assert!(validate_redirect_query("").is_ok());
    assert!(validate_redirect_query("next=#{path}&q=#{query}").is_ok());
    assert!(validate_redirect_query("proto=#{protocol}&host=#{host}&port=#{port}").is_ok());
    assert!(validate_redirect_query("spaces and arbitrary chars !%[]").is_ok());
    assert!(validate_redirect_query(&"a".repeat(128)).is_ok());
    // Confirm every documented placeholder is accepted.
    assert!(validate_redirect_query("#{protocol}").is_ok());
    assert!(validate_redirect_query("#{host}").is_ok());
    assert!(validate_redirect_query("#{port}").is_ok());
    assert!(validate_redirect_query("#{path}").is_ok());
    assert!(validate_redirect_query("#{query}").is_ok());

    assert!(validate_redirect_query(&"a".repeat(129)).is_err());
    assert!(validate_redirect_query("#{bogus}").is_err());
}

#[test]
fn validate_http_response_status_code_type() {
    let t = types::http_response_status_code();

    // Type identity: refined String with kind "HttpResponseStatusCode"
    match t.kind() {
        AttrTypeKind::String { identity, .. } => {
            assert_eq!(
                identity.as_ref().map(|id| id.kind.as_str()),
                Some("HttpResponseStatusCode")
            );
        }
        other => panic!("Expected refined String, got: {:?}", other),
    }

    // Valid
    for ok in ["200", "299", "400", "499", "500", "599"] {
        assert!(
            t.validate(&Value::Concrete(ConcreteValue::String(ok.to_string())))
                .is_ok(),
            "Expected {ok} to validate"
        );
    }

    // Invalid (includes fence-post boundaries 199/300/399 for the
    // excluded 1XX/3XX ranges).
    for bad in [
        "100", "199", "300", "301", "399", "600", "nonsense", "20", "2000", "12a", "",
    ] {
        assert!(
            t.validate(&Value::Concrete(ConcreteValue::String(bad.to_string())))
                .is_err(),
            "Expected {bad} to fail validation"
        );
    }

    // Wrong type
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::Int(42)))
            .is_err()
    );
}

#[cfg(test)]
mod schema_validate_attr_struct_tests {
    use super::*;
    use indexmap::IndexMap;

    fn struct_type(name: &str, fields: Vec<StructField>) -> AttributeType {
        AttributeType::struct_(name.to_string(), fields)
    }

    fn map_value(entries: Vec<(&str, Value)>) -> Value {
        let mut map = IndexMap::new();
        for (key, value) in entries {
            map.insert(key.to_string(), value);
        }
        Value::Concrete(ConcreteValue::Map(map))
    }

    #[test]
    fn validate_attr_rejects_unknown_nested_struct_field_with_suggestion() {
        let retention = struct_type(
            "Retention",
            vec![StructField::new("days", AttributeType::int())],
        );
        let policy = struct_type(
            "Policy",
            vec![StructField::new("retention", retention).required()],
        );
        let value = map_value(vec![(
            "retention",
            map_value(vec![("dayz", Value::Concrete(ConcreteValue::Int(30)))]),
        )]);

        let err = Schema::flat(AttributeType::string())
            .validate_attr(&policy, &value)
            .expect_err("Schema::validate_attr must reject unknown nested struct fields");

        match err {
            TypeError::StructFieldError { field, inner } => {
                assert_eq!(field, "retention");
                assert_eq!(
                    *inner,
                    TypeError::UnknownStructField {
                        struct_name: "Retention".to_string(),
                        field: "dayz".to_string(),
                        suggestion: Some("days".to_string()),
                    }
                );
            }
            other => panic!("expected nested UnknownStructField, got {other:?}"),
        }
    }

    #[test]
    fn validate_collect_discriminates_union_of_single_field_structs() {
        let retention = AttributeType::union(vec![
            struct_type(
                "RetentionDays",
                vec![StructField::new("days", AttributeType::int()).required()],
            ),
            struct_type(
                "RetentionYears",
                vec![StructField::new("years", AttributeType::int()).required()],
            ),
        ]);
        let schema = Schema::flat(retention);
        let days = map_value(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);
        let years = map_value(vec![("years", Value::Concrete(ConcreteValue::Int(1)))]);
        let mixed = map_value(vec![
            ("days", Value::Concrete(ConcreteValue::Int(30))),
            ("years", Value::Concrete(ConcreteValue::Int(1))),
        ]);

        assert!(
            schema.validate_collect(&days).is_empty(),
            "the RetentionDays member must accept its declared field"
        );
        assert!(
            schema.validate_collect(&years).is_empty(),
            "the RetentionYears member must accept its declared field"
        );

        let errors = schema.validate_collect(&mixed);
        assert_eq!(errors.len(), 1, "mixed union value must be rejected");
        let TypeError::UnionStructMismatch {
            reason,
            supplied_fields,
            alternatives,
        } = &errors[0].1
        else {
            panic!("expected UnionStructMismatch, got {:?}", errors[0].1)
        };
        assert_eq!(reason, &UnionStructMismatchReason::ConflictingAlternatives);
        assert_eq!(supplied_fields, &["days", "years"]);
        assert_eq!(
            alternatives
                .iter()
                .map(|alternative| alternative.name.as_str())
                .collect::<Vec<_>>(),
            vec!["RetentionDays", "RetentionYears"]
        );
    }

    #[test]
    fn validate_attr_accepts_and_type_checks_block_name_alias() {
        let lifecycle = struct_type(
            "Lifecycle",
            vec![
                StructField::new("transitions", AttributeType::int()).with_block_name("transition"),
            ],
        );
        let schema = Schema::flat(AttributeType::string());
        let valid = map_value(vec![(
            "transition",
            Value::Concrete(ConcreteValue::Int(30)),
        )]);

        assert!(schema.validate_attr(&lifecycle, &valid).is_ok());

        let invalid = map_value(vec![(
            "transition",
            Value::Concrete(ConcreteValue::String("thirty".to_string())),
        )]);
        let err = schema
            .validate_attr(&lifecycle, &invalid)
            .expect_err("the accepted alias must resolve to its field type");
        assert!(matches!(
            err,
            TypeError::StructFieldError { field, inner }
                if field == "transition" && matches!(*inner, TypeError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn validate_attr_preserves_required_and_nested_type_errors() {
        let inner = struct_type(
            "Inner",
            vec![StructField::new("count", AttributeType::int()).required()],
        );
        let outer = struct_type("Outer", vec![StructField::new("nested", inner).required()]);
        let schema = Schema::flat(AttributeType::string());

        let missing = schema
            .validate_attr(&outer, &map_value(vec![]))
            .expect_err("the required outer field must remain required");
        assert_eq!(
            missing,
            TypeError::MissingRequired {
                name: "nested".to_string()
            }
        );

        let wrong_type = map_value(vec![(
            "nested",
            map_value(vec![(
                "count",
                Value::Concrete(ConcreteValue::String("one".to_string())),
            )]),
        )]);
        let err = schema
            .validate_attr(&outer, &wrong_type)
            .expect_err("nested field type errors must remain recursive");
        match err {
            TypeError::StructFieldError { field, inner } => {
                assert_eq!(field, "nested");
                assert!(matches!(
                    *inner,
                    TypeError::StructFieldError { field, inner }
                        if field == "count" && matches!(*inner, TypeError::TypeMismatch { .. })
                ));
            }
            other => panic!("expected nested StructFieldError, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod validate_collect_tests {
    use super::*;
    use indexmap::IndexMap;

    fn struct_type(name: &str, fields: Vec<StructField>) -> AttributeType {
        AttributeType::struct_(name.to_string(), fields)
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
                StructField::new("status", AttributeType::string()).required(),
                StructField::new("mfa_delete", AttributeType::bool()),
            ],
        );
        let v = map_value(vec![
            (
                "status",
                Value::Concrete(ConcreteValue::String("Enabled".to_string())),
            ),
            ("mfa_delete", Value::Concrete(ConcreteValue::Bool(false))),
        ]);
        let errors = crate::schema::Schema::flat(ty.clone()).validate_collect(&v);
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
            vec![StructField::new("status", AttributeType::string())],
        );
        let v = map_value(vec![
            (
                "statuus",
                Value::Concrete(ConcreteValue::String("Enabled".to_string())),
            ),
            ("mfa", Value::Concrete(ConcreteValue::Bool(false))),
        ]);
        let errors = crate::schema::Schema::flat(ty.clone()).validate_collect(&v);
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
            vec![StructField::new("count", AttributeType::int()).required()],
        );
        let outer = struct_type("Outer", vec![StructField::new("nested", inner).required()]);
        let v = map_value(vec![(
            "nested",
            map_value(vec![(
                "count",
                Value::Concrete(ConcreteValue::String("not an int".to_string())),
            )]),
        )]);
        let errors = crate::schema::Schema::flat(outer.clone()).validate_collect(&v);
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
            vec![StructField::new("name", AttributeType::string()).required()],
        );
        let outer_attr = AttributeType::list(inner);

        // Two list items, second one has wrong type for `name`
        let v = Value::Concrete(ConcreteValue::List(vec![
            map_value(vec![(
                "name",
                Value::Concrete(ConcreteValue::String("ok".to_string())),
            )]),
            map_value(vec![("name", Value::Concrete(ConcreteValue::Int(42)))]),
        ]));
        let errors = crate::schema::Schema::flat(outer_attr.clone()).validate_collect(&v);
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
                StructField::new("transitions", AttributeType::string())
                    .with_block_name("transition"),
            ],
        );
        let v = map_value(vec![(
            "transition",
            Value::Concrete(ConcreteValue::String("ok".to_string())),
        )]);
        let errors = crate::schema::Schema::flat(ty.clone()).validate_collect(&v);
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
            vec![StructField::new("vpc_id", AttributeType::int())],
        );
        let v = map_value(vec![(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
        )]);
        let errors = crate::schema::Schema::flat(ty.clone()).validate_collect(&v);
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
            vec![StructField::new("status", AttributeType::string())],
        );
        let v = map_value(vec![(
            "statuus",
            Value::Concrete(ConcreteValue::String("x".to_string())),
        )]);
        let errors = crate::schema::Schema::flat(ty.clone()).validate_collect(&v);
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
    let v = ExpectedEnumVariant::from_namespaced(
        Some("awscc.sso.Assignment"),
        "TargetType",
        "AWS_ACCOUNT",
        false,
    );
    assert_eq!(v.to_string(), "awscc.sso.Assignment.TargetType.AWS_ACCOUNT");
}

#[test]
fn expected_enum_variant_display_bare_when_no_provider() {
    // Non-namespaced enums (`provider = None`) must render only the
    // bare value — matching how the legacy formatter pushed `v.to_string()`
    // when `namespace` was `None`.
    let v = ExpectedEnumVariant::from_namespaced(None, "Mode", "fast", false);
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
    // When the schema declares a 1:1 `to_dsl` mapping for canonical
    // values, `expected` stores only the DSL spelling. These entries are
    // still canonical suggestions (`is_alias = false`) because they are
    // the only spellings users can type.
    let t = AttributeType::enum_(
        crate::schema::enum_identity("VersioningStatus", Some("aws.s3.Bucket")),
        Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
        vec![
            ("Enabled".to_string(), "enabled".to_string()),
            ("Suspended".to_string(), "suspended".to_string()),
        ],
        None,
        None,
    );
    // Use `EnumIdentifier` so the strict-mode validator reaches the
    // wrong-variant branch (`InvalidEnumVariant`). A `String` here would
    // be rejected earlier as `StringLiteralExpectedEnum` — that path is
    // covered by `validate_enum_rejects_quoted_string_literal`.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
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
        vec!["enabled", "suspended"],
    );
    assert!(aliases.is_empty(), "1:1 to_dsl aliases must collapse");
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
            AttributeType::enum_with_base(
                crate::schema::enum_identity("Mode", Some("test.r")),
                AttributeType::string(),
                None,
                vec![],
                Some(legacy_validator(validate_mode)),
                None,
            ),
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
    let original = ExpectedEnumVariant::from_namespaced(
        Some("awscc.sso.Assignment"),
        "TargetType",
        "AWS_ACCOUNT",
        false,
    );
    let json = serde_json::to_value(&original).expect("serialize");
    assert!(
        json.get("value").is_some_and(serde_json::Value::is_string),
        "DslSpelling must serialize as a bare string inside ExpectedEnumVariant"
    );
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
// Enum/Custom) wins over an unrelated member. Unique failures stay
// verbatim; tied closed-Struct failures become an alternatives diagnostic.

fn retention_union() -> AttributeType {
    AttributeType::union(vec![
        AttributeType::struct_(
            "RetentionDays".to_string(),
            vec![StructField::new("days", AttributeType::int()).required()],
        ),
        AttributeType::struct_(
            "RetentionYears".to_string(),
            vec![StructField::new("years", AttributeType::int()).required()],
        ),
    ])
}

fn retention_ref_schema() -> Schema {
    Schema {
        root: AttributeType::union(vec![
            AttributeType::ref_("RetentionDays"),
            AttributeType::ref_("RetentionYears"),
        ]),
        defs: std::collections::BTreeMap::from([
            (
                "RetentionDays".to_string(),
                required_int_struct("RetentionDays", &["days"]),
            ),
            (
                "RetentionYears".to_string(),
                required_int_struct("RetentionYears", &["years"]),
            ),
        ]),
    }
}

fn required_int_struct(name: &str, fields: &[&str]) -> AttributeType {
    AttributeType::struct_(
        name.to_string(),
        fields
            .iter()
            .map(|field| StructField::new(*field, AttributeType::int()).required())
            .collect(),
    )
}

fn union_map(entries: Vec<(&str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        entries
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    ))
}

/// Exercise all public validation paths that own Union diagnostics. Keeping
/// them in every assertion prevents either the schema-aware CLI/LSP path or
/// the standalone path from silently retaining the old first-arm behavior.
fn union_errors_from_public_paths(
    union: &AttributeType,
    value: &Value,
) -> Vec<(&'static str, TypeError)> {
    let schema = Schema::flat(union.clone());
    let standalone = union
        .validate(value)
        .expect_err("AttributeType::validate must reject the value");
    let schema_attr = schema
        .validate_attr(union, value)
        .expect_err("Schema::validate_attr must reject the value");
    let collected = schema.validate_collect(value);
    assert_eq!(
        collected.len(),
        1,
        "Schema::validate_collect must produce one union-level error, got {collected:?}"
    );

    vec![
        ("AttributeType::validate", standalone),
        ("Schema::validate_attr", schema_attr),
        ("Schema::validate_collect", collected[0].1.clone()),
    ]
}

fn union_ref_errors_from_schema_paths(
    schema: &Schema,
    value: &Value,
) -> Vec<(&'static str, TypeError)> {
    let schema_root = schema
        .validate(value)
        .expect_err("Schema::validate must reject the value");
    let schema_attr = schema
        .validate_attr(&schema.root, value)
        .expect_err("Schema::validate_attr must reject the value");
    let collected = schema.validate_collect(value);
    assert_eq!(
        collected.len(),
        1,
        "Schema::validate_collect must produce one union-level error, got {collected:?}"
    );

    vec![
        ("Schema::validate", schema_root),
        ("Schema::validate_attr", schema_attr),
        ("Schema::validate_collect", collected[0].1.clone()),
    ]
}

fn assert_union_ref_valid_through_schema_paths(schema: &Schema, value: &Value) {
    assert!(
        schema.validate(value).is_ok(),
        "Schema::validate rejected {value:?}"
    );
    assert!(
        schema.validate_attr(&schema.root, value).is_ok(),
        "Schema::validate_attr rejected {value:?}"
    );
    assert!(
        schema.validate_collect(value).is_empty(),
        "Schema::validate_collect rejected {value:?}"
    );
}

fn assert_union_valid_through_public_paths(union: &AttributeType, value: &Value) {
    let schema = Schema::flat(union.clone());
    assert!(
        union.validate(value).is_ok(),
        "AttributeType::validate rejected {value:?}"
    );
    assert!(
        schema.validate_attr(union, value).is_ok(),
        "Schema::validate_attr rejected {value:?}"
    );
    assert!(
        schema.validate_collect(value).is_empty(),
        "Schema::validate_collect rejected {value:?}"
    );
}

fn assert_retention_alternative_metadata(path: &str, alternatives: &[UnionStructAlternative]) {
    assert_eq!(alternatives.len(), 2, "{path} lost a tied alternative");
    assert_eq!(alternatives[0].name, "RetentionDays");
    assert_eq!(alternatives[0].accepted_fields, vec!["days".to_string()]);
    assert_eq!(alternatives[0].required_fields, vec!["days".to_string()]);
    assert_eq!(alternatives[1].name, "RetentionYears");
    assert_eq!(alternatives[1].accepted_fields, vec!["years".to_string()]);
    assert_eq!(alternatives[1].required_fields, vec!["years".to_string()]);
}

#[test]
fn union_struct_tie_reports_fields_from_multiple_alternatives() {
    let union = retention_union();
    let value = union_map(vec![
        ("days", Value::Concrete(ConcreteValue::Int(30))),
        ("years", Value::Concrete(ConcreteValue::Int(1))),
    ]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let TypeError::UnionStructMismatch {
            reason,
            supplied_fields,
            alternatives,
        } = &err
        else {
            panic!("{path} must return structured UnionStructMismatch, got {err:?}")
        };
        assert_eq!(reason, &UnionStructMismatchReason::ConflictingAlternatives);
        assert_eq!(supplied_fields, &["days", "years"]);
        assert_retention_alternative_metadata(path, alternatives);
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days' or 'years', but both were supplied",
            "{path} must render the mixed-alternative diagnostic concisely"
        );
    }
}

#[test]
fn union_struct_diagnostic_uses_recognized_field_sets_not_cardinality() {
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "Days".to_string(),
            vec![
                StructField::new("days", AttributeType::int()).required(),
                StructField::new("unit", AttributeType::string()),
            ],
        ),
        required_int_struct("Years", &["years"]),
    ]);
    let value = union_map(vec![
        ("days", Value::Concrete(ConcreteValue::Int(30))),
        ("years", Value::Concrete(ConcreteValue::Int(1))),
        (
            "unit",
            Value::Concrete(ConcreteValue::String("calendar".to_string())),
        ),
    ]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert!(
            matches!(
                &err,
                TypeError::UnionStructMismatch {
                    reason: UnionStructMismatchReason::ConflictingAlternatives,
                    ..
                }
            ),
            "{path} must retain both non-empty recognized field sets"
        );
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days' or 'years', but both were supplied",
            "{path} must not choose Days merely because it recognizes two fields"
        );
    }
}

#[test]
fn union_struct_superset_recognized_set_does_not_hide_other_alternatives() {
    let union = AttributeType::union(vec![
        required_int_struct("Days", &["days"]),
        required_int_struct("Years", &["years"]),
        required_int_struct("Both", &["days", "years", "tz"]),
    ]);
    let value = union_map(vec![
        ("days", Value::Concrete(ConcreteValue::Int(30))),
        ("years", Value::Concrete(ConcreteValue::Int(1))),
    ]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert!(
            matches!(&err, TypeError::UnionStructMismatch { .. }),
            "{path} must retain the Days and Years alternatives"
        );
        assert_eq!(
            err.to_string(),
            "Expected one of these forms: 'days', 'years', or ('days', 'years', and 'tz')",
            "{path} must not choose Both merely because it recognizes both supplied fields"
        );
    }
}

#[test]
fn union_struct_tie_reports_no_alternative_for_empty_map() {
    let union = retention_union();
    let value = union_map(vec![]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let TypeError::UnionStructMismatch {
            reason,
            supplied_fields,
            alternatives,
        } = &err
        else {
            panic!("{path} must return structured UnionStructMismatch, got {err:?}")
        };
        assert_eq!(
            reason,
            &UnionStructMismatchReason::NoAlternativeSatisfied {
                unrecognized_fields: vec![]
            }
        );
        assert!(supplied_fields.is_empty());
        assert_retention_alternative_metadata(path, alternatives);
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days' or 'years', but none were supplied",
            "{path} must render the empty-alternative diagnostic concisely"
        );
    }
}

#[test]
fn union_struct_tie_reports_unrecognized_fields_and_all_alternatives() {
    let union = retention_union();
    let value = union_map(vec![("months", Value::Concrete(ConcreteValue::Int(3)))]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let TypeError::UnionStructMismatch {
            reason,
            supplied_fields,
            alternatives,
        } = &err
        else {
            panic!("{path} must return structured UnionStructMismatch, got {err:?}")
        };
        assert_eq!(
            reason,
            &UnionStructMismatchReason::NoAlternativeSatisfied {
                unrecognized_fields: vec!["months".to_string()]
            }
        );
        assert_eq!(supplied_fields, &["months"]);
        assert_retention_alternative_metadata(path, alternatives);
        assert_eq!(
            err.to_string(),
            "Unknown field 'months': expected exactly one of 'days' or 'years'",
            "{path} must render the unknown-field diagnostic concisely"
        );
    }
}

#[test]
fn union_struct_three_alternatives_render_as_one_readable_list() {
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "RetentionDays".to_string(),
            vec![StructField::new("days", AttributeType::int()).required()],
        ),
        AttributeType::struct_(
            "RetentionWeeks".to_string(),
            vec![StructField::new("weeks", AttributeType::int()).required()],
        ),
        AttributeType::struct_(
            "RetentionYears".to_string(),
            vec![StructField::new("years", AttributeType::int()).required()],
        ),
    ]);
    let value = union_map(vec![]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days', 'weeks', or 'years', but none were supplied",
            "{path} must render three alternatives as a readable list"
        );
    }
}

#[test]
fn union_struct_multi_field_alternatives_render_each_field_set_once() {
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "DateRange".to_string(),
            vec![
                StructField::new("start", AttributeType::string()).required(),
                StructField::new("end", AttributeType::string()).required(),
            ],
        ),
        AttributeType::struct_(
            "DayMode".to_string(),
            vec![
                StructField::new("days", AttributeType::int()).required(),
                StructField::new("mode", AttributeType::string()).required(),
            ],
        ),
    ]);
    let value = union_map(vec![]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Expected exactly one of ('start' and 'end') or ('days' and 'mode'), but none were supplied",
            "{path} must group each multi-field alternative without dumping metadata"
        );
    }
}

#[test]
fn union_struct_shared_required_field_advice_names_complete_valid_forms() {
    let union = AttributeType::union(vec![
        required_int_struct("A", &["a", "b"]),
        required_int_struct("B", &["b", "c"]),
    ]);
    let expected = "Expected one of these forms: ('a' and 'b') or ('b' and 'c')";

    for value in [
        union_map(vec![]),
        union_map(vec![
            ("a", Value::Concrete(ConcreteValue::Int(1))),
            ("b", Value::Concrete(ConcreteValue::Int(2))),
            ("c", Value::Concrete(ConcreteValue::Int(3))),
        ]),
    ] {
        for (path, err) in union_errors_from_public_paths(&union, &value) {
            assert_eq!(
                err.to_string(),
                expected,
                "{path} must retain the shared required field in every advised form"
            );
        }
    }

    let corrected_a = union_map(vec![
        ("a", Value::Concrete(ConcreteValue::Int(1))),
        ("b", Value::Concrete(ConcreteValue::Int(2))),
    ]);
    let corrected_b = union_map(vec![
        ("b", Value::Concrete(ConcreteValue::Int(2))),
        ("c", Value::Concrete(ConcreteValue::Int(3))),
    ]);
    assert_union_valid_through_public_paths(&union, &corrected_a);
    assert_union_valid_through_public_paths(&union, &corrected_b);
}

#[test]
fn union_struct_superset_arms_do_not_claim_xor() {
    let union = AttributeType::union(vec![
        required_int_struct("Days", &["days"]),
        required_int_struct("DaysWithExtra", &["days", "extra"]),
    ]);
    let value = union_map(vec![]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Expected one of these forms: 'days' or ('days' and 'extra')",
            "{path} must not claim mutually exclusive forms for a subset relation"
        );
    }

    let days = union_map(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);
    let days_with_extra = union_map(vec![
        ("days", Value::Concrete(ConcreteValue::Int(30))),
        ("extra", Value::Concrete(ConcreteValue::Int(1))),
    ]);
    assert_union_valid_through_public_paths(&union, &days);
    assert_union_valid_through_public_paths(&union, &days_with_extra);
}

#[test]
fn union_struct_overlapping_optional_arms_do_not_demand_optional_fields() {
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "DaysWithExtra".to_string(),
            vec![
                StructField::new("days", AttributeType::int()).required(),
                StructField::new("extra", AttributeType::int()),
            ],
        ),
        AttributeType::struct_(
            "DaysWithOther".to_string(),
            vec![
                StructField::new("days", AttributeType::int()).required(),
                StructField::new("other", AttributeType::int()),
            ],
        ),
    ]);
    let value = union_map(vec![]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Expected one of these forms: 'days' (`DaysWithExtra`) or 'days' (`DaysWithOther`)",
            "{path} must name the shared required field without demanding either optional field"
        );
    }

    let corrected = union_map(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);
    assert_union_valid_through_public_paths(&union, &corrected);
}

#[test]
fn union_struct_partial_conflict_lists_every_declared_arm() {
    let union = AttributeType::union(vec![
        required_int_struct("RetentionDays", &["days"]),
        required_int_struct("RetentionWeeks", &["weeks"]),
        required_int_struct("RetentionMonths", &["months"]),
        required_int_struct("RetentionYears", &["years"]),
        required_int_struct("RetentionDecades", &["decades"]),
    ]);
    let value = union_map(vec![
        ("days", Value::Concrete(ConcreteValue::Int(30))),
        ("years", Value::Concrete(ConcreteValue::Int(1))),
    ]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let TypeError::UnionStructMismatch { alternatives, .. } = &err else {
            panic!("{path} must return UnionStructMismatch, got {err:?}")
        };
        assert_eq!(
            alternatives
                .iter()
                .map(|alternative| alternative.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "RetentionDays",
                "RetentionWeeks",
                "RetentionMonths",
                "RetentionYears",
                "RetentionDecades"
            ],
            "{path} must carry every declared arm, not only tied failures"
        );
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days', 'weeks', 'months', 'years', or 'decades', but more than one was supplied",
            "{path} must not narrow the declared options to the tied arms"
        );
    }

    let corrected = union_map(vec![("months", Value::Concrete(ConcreteValue::Int(3)))]);
    assert_union_valid_through_public_paths(&union, &corrected);
}

#[test]
fn union_struct_four_by_four_rendering_is_bounded() {
    let union = AttributeType::union(vec![
        required_int_struct(
            "Alpha",
            &["alpha_one", "alpha_two", "alpha_three", "alpha_four"],
        ),
        required_int_struct(
            "Bravo",
            &["bravo_one", "bravo_two", "bravo_three", "bravo_four"],
        ),
        required_int_struct(
            "Charlie",
            &[
                "charlie_one",
                "charlie_two",
                "charlie_three",
                "charlie_four",
            ],
        ),
        required_int_struct(
            "Delta",
            &["delta_one", "delta_two", "delta_three", "delta_four"],
        ),
    ]);
    let value = union_map(vec![]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let message = err.to_string();
        assert_eq!(
            message,
            "Expected exactly one of ('alpha_one', 'alpha_two', 'alpha_three', and 'alpha_four') or 3 more alternatives, but none were supplied",
            "{path} must elide whole alternatives without truncating a field name"
        );
        assert!(
            message.chars().count() <= 160,
            "{path} rendered an unbounded {}-character message: {message}",
            message.chars().count()
        );
        assert!(!message.contains(['\n', ';']));
    }
}

#[test]
fn union_struct_long_field_rendering_is_bounded() {
    let union = AttributeType::union(vec![
        required_int_struct(
            "PrimaryRetentionForm",
            &[
                "primary_retention_period_in_calendar_days",
                "primary_retention_archive_transition_policy",
            ],
        ),
        required_int_struct(
            "SecondaryRetentionForm",
            &[
                "secondary_retention_period_in_calendar_years",
                "secondary_retention_archive_transition_policy",
            ],
        ),
    ]);
    let value = union_map(vec![]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let message = err.to_string();
        assert_eq!(
            message,
            "Expected exactly one of `PrimaryRetentionForm` or `SecondaryRetentionForm`, but none were supplied",
            "{path} must use visibly quoted type names when field sets cannot fit"
        );
        assert!(
            message.chars().count() <= 160,
            "{path} rendered an unbounded {}-character message: {message}",
            message.chars().count()
        );
        assert!(!message.contains(['\n', ';']));
    }
}

#[test]
fn union_struct_unknown_field_restores_similar_name_suggestion() {
    let union = retention_union();
    let value = union_map(vec![("dayz", Value::Concrete(ConcreteValue::Int(30)))]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Unknown field 'dayz', did you mean 'days'? Expected exactly one of 'days' or 'years'",
            "{path} must preserve the sibling struct diagnostic's typo suggestion"
        );
    }

    let corrected = union_map(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);
    assert_union_valid_through_public_paths(&union, &corrected);
}

#[test]
fn union_struct_empty_arm_name_is_visibly_a_type_name() {
    let union = AttributeType::union(vec![
        AttributeType::struct_("EmptyArm".to_string(), vec![]),
        required_int_struct("RetentionDays", &["days"]),
    ]);
    let value = union_map(vec![("zzz", Value::Concrete(ConcreteValue::Int(1)))]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Unknown field 'zzz': expected one of these forms: empty `EmptyArm` form or 'days'",
            "{path} must distinguish a struct name from field names"
        );
    }

    assert_union_valid_through_public_paths(&union, &union_map(vec![]));
}

#[test]
fn union_struct_ambiguous_field_sets_quote_type_names() {
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "IntVariant".to_string(),
            vec![StructField::new("value", AttributeType::int()).required()],
        ),
        AttributeType::struct_(
            "StringVariant".to_string(),
            vec![StructField::new("value", AttributeType::string()).required()],
        ),
    ]);
    let value = union_map(vec![("value", Value::Concrete(ConcreteValue::Bool(true)))]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Expected one of these forms: 'value' (`IntVariant`) or 'value' (`StringVariant`)",
            "{path} must visibly distinguish type names from fields"
        );
    }
}

#[test]
fn union_struct_tie_with_only_shared_fields_reports_no_satisfied_alternative() {
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "RetentionDays".to_string(),
            vec![
                StructField::new("label", AttributeType::string()).required(),
                StructField::new("days", AttributeType::int()).required(),
            ],
        ),
        AttributeType::struct_(
            "RetentionYears".to_string(),
            vec![
                StructField::new("label", AttributeType::string()).required(),
                StructField::new("years", AttributeType::int()).required(),
            ],
        ),
    ]);
    let value = union_map(vec![(
        "label",
        Value::Concrete(ConcreteValue::String("archive".to_string())),
    )]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let TypeError::UnionStructMismatch { reason, .. } = &err else {
            panic!("{path} must return UnionStructMismatch, got {err:?}")
        };
        assert_eq!(
            reason,
            &UnionStructMismatchReason::NoAlternativeSatisfied {
                unrecognized_fields: vec![]
            },
            "{path} must not call a field accepted by every arm conflicting"
        );
        assert_eq!(
            err.to_string(),
            "Expected one of these forms: ('label' and 'days') or ('label' and 'years')",
            "{path} must retain the shared required field in every form"
        );
    }

    let corrected = union_map(vec![
        (
            "label",
            Value::Concrete(ConcreteValue::String("archive".to_string())),
        ),
        ("days", Value::Concrete(ConcreteValue::Int(30))),
    ]);
    assert_union_valid_through_public_paths(&union, &corrected);
}

#[test]
fn union_struct_accepts_days_alternative_through_all_public_paths() {
    let union = retention_union();
    let value = union_map(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);
    assert_union_valid_through_public_paths(&union, &value);
}

#[test]
fn union_struct_accepts_years_alternative_through_all_public_paths() {
    let union = retention_union();
    let value = union_map(vec![("years", Value::Concrete(ConcreteValue::Int(1)))]);
    assert_union_valid_through_public_paths(&union, &value);
}

#[test]
fn union_struct_unique_field_match_preserves_detailed_member_error_verbatim() {
    let union = retention_union();
    let value = union_map(vec![(
        "days",
        Value::Concrete(ConcreteValue::String("x".to_string())),
    )]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Struct field 'days': Type mismatch: expected Int, got String",
            "{path} must preserve the unique RetentionDays member error"
        );
    }
}

// ---------------------------------------------------------------------------
// #3757 — Ref-based Union members use the shared structural judgement
// ---------------------------------------------------------------------------

#[test]
fn schema_union_ref_struct_tie_reports_fields_from_multiple_alternatives() {
    let schema = retention_ref_schema();
    let value = union_map(vec![
        ("days", Value::Concrete(ConcreteValue::Int(30))),
        ("years", Value::Concrete(ConcreteValue::Int(1))),
    ]);

    for (path, err) in union_ref_errors_from_schema_paths(&schema, &value) {
        let TypeError::UnionStructMismatch {
            reason,
            supplied_fields,
            alternatives,
        } = &err
        else {
            panic!("{path} must return structured UnionStructMismatch, got {err:?}")
        };
        assert_eq!(reason, &UnionStructMismatchReason::ConflictingAlternatives);
        assert_eq!(supplied_fields, &["days", "years"]);
        assert_retention_alternative_metadata(path, alternatives);
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days' or 'years', but both were supplied",
            "{path} must match the inline-Struct diagnostic"
        );
    }
}

#[test]
fn schema_union_ref_struct_tie_reports_no_alternative_for_empty_map() {
    let schema = retention_ref_schema();
    let value = union_map(vec![]);

    for (path, err) in union_ref_errors_from_schema_paths(&schema, &value) {
        let TypeError::UnionStructMismatch {
            reason,
            supplied_fields,
            alternatives,
        } = &err
        else {
            panic!("{path} must return structured UnionStructMismatch, got {err:?}")
        };
        assert_eq!(
            reason,
            &UnionStructMismatchReason::NoAlternativeSatisfied {
                unrecognized_fields: vec![]
            }
        );
        assert!(supplied_fields.is_empty());
        assert_retention_alternative_metadata(path, alternatives);
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days' or 'years', but none were supplied",
            "{path} must match the inline-Struct diagnostic"
        );
    }
}

#[test]
fn schema_union_ref_struct_unique_field_match_preserves_detailed_member_error_verbatim() {
    let schema = retention_ref_schema();
    let value = union_map(vec![(
        "days",
        Value::Concrete(ConcreteValue::String("x".to_string())),
    )]);

    for (path, err) in union_ref_errors_from_schema_paths(&schema, &value) {
        assert_eq!(
            err.to_string(),
            "Struct field 'days': Type mismatch: expected Int, got String",
            "{path} must preserve the unique RetentionDays member error"
        );
    }
}

#[test]
fn schema_union_ref_struct_unknown_field_restores_similar_name_suggestion() {
    let schema = retention_ref_schema();
    let value = union_map(vec![("dayz", Value::Concrete(ConcreteValue::Int(30)))]);

    for (path, err) in union_ref_errors_from_schema_paths(&schema, &value) {
        assert_eq!(
            err.to_string(),
            "Unknown field 'dayz', did you mean 'days'? Expected exactly one of 'days' or 'years'",
            "{path} must match the inline-Struct typo diagnostic"
        );
    }

    let corrected = union_map(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);
    assert_union_ref_valid_through_schema_paths(&schema, &corrected);
}

#[test]
fn schema_union_ref_member_missing_from_defs_keeps_generic_type_mismatch() {
    let schema = Schema::flat(AttributeType::union(vec![
        AttributeType::ref_("MissingDays"),
        AttributeType::ref_("MissingYears"),
    ]));
    let value = union_map(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);
    let expected = TypeError::TypeMismatch {
        expected: schema.root.type_name(),
        got: "Map".to_string(),
    };

    for (path, err) in union_ref_errors_from_schema_paths(&schema, &value) {
        assert_eq!(err, expected, "{path} changed the missing-Ref fallback");
        assert!(
            !err.to_string().contains("internal:"),
            "{path} leaked an implementation detail"
        );
    }
}

#[test]
fn standalone_union_ref_members_keep_generic_type_mismatch_without_internal_error() {
    let union = AttributeType::union(vec![
        AttributeType::ref_("RetentionDays"),
        AttributeType::ref_("RetentionYears"),
    ]);
    let value = union_map(vec![("days", Value::Concrete(ConcreteValue::Int(30)))]);

    let err = union
        .validate(&value)
        .expect_err("standalone Ref members cannot resolve without defs");
    assert_eq!(
        err,
        TypeError::TypeMismatch {
            expected: union.type_name(),
            got: "Map".to_string(),
        }
    );
    assert!(!err.to_string().contains("internal:"));
}

#[test]
fn schema_validate_self_cyclic_union_ref_failing_value_terminates() {
    let schema = Schema {
        root: AttributeType::ref_("A"),
        defs: std::collections::BTreeMap::from([(
            "A".to_string(),
            AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::string()]),
        )]),
    };
    let value = Value::Concrete(ConcreteValue::Int(1));

    let err = schema
        .validate(&value)
        .expect_err("the cyclic union has no member accepting Int");
    assert_eq!(
        err,
        TypeError::TypeMismatch {
            expected: "Ref(A) | String".to_string(),
            got: "Int".to_string(),
        }
    );
}

#[test]
fn schema_validate_ref_alias_cycle_terminates_for_validate_and_collect() {
    let schema = Schema {
        root: AttributeType::ref_("A"),
        defs: std::collections::BTreeMap::from([
            ("A".to_string(), AttributeType::ref_("B")),
            ("B".to_string(), AttributeType::ref_("A")),
        ]),
    };
    let value = Value::Concrete(ConcreteValue::Int(1));
    let expected = TypeError::ValidationFailed {
        message: "cyclic schema reference `A`".to_string(),
    };

    assert_eq!(schema.validate(&value), Err(expected.clone()));
    let collected = schema.validate_collect(&value);
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].1, expected);
}

#[test]
fn schema_validation_ref_cycle_guard_resets_for_struct_field_values() {
    let schema = Schema {
        root: AttributeType::ref_("Node"),
        defs: std::collections::BTreeMap::from([(
            "Node".to_string(),
            AttributeType::struct_(
                "Node",
                vec![StructField::new("child", AttributeType::ref_("Node"))],
            ),
        )]),
    };
    let value = union_map(vec![("child", union_map(vec![]))]);

    assert!(schema.validate(&value).is_ok());
    assert!(schema.validate_collect(&value).is_empty());
}

#[test]
fn schema_union_judgement_resets_ref_path_for_list_failure_attribution() {
    let schema = Schema {
        root: AttributeType::ref_("A"),
        defs: std::collections::BTreeMap::from([(
            "A".to_string(),
            AttributeType::union(vec![
                AttributeType::list(AttributeType::int()),
                AttributeType::list(AttributeType::ref_("A")),
                AttributeType::string(),
            ]),
        )]),
    };
    let value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::List(vec![Value::Concrete(ConcreteValue::Bool(true))]),
    )]));

    let TypeError::ListItemError { index: 0, inner } = schema
        .validate(&value)
        .expect_err("the recursive list value must fail validation")
    else {
        panic!("the recursive List member must own failure attribution")
    };
    assert!(
        matches!(*inner, TypeError::ListItemError { index: 0, .. }),
        "the recursive List member must preserve the nested item error, got {inner:?}"
    );
}

#[test]
fn nested_union_preserves_inner_struct_alternative_diagnostic() {
    let union = AttributeType::union(vec![retention_union(), AttributeType::bool()]);
    let value = union_map(vec![
        ("days", Value::Concrete(ConcreteValue::Int(30))),
        ("years", Value::Concrete(ConcreteValue::Int(1))),
    ]);

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err.to_string(),
            "Expected exactly one of 'days' or 'years', but both were supplied",
            "{path} discarded or changed the nested union diagnostic"
        );
    }
}

#[test]
fn scalar_union_without_a_shape_match_keeps_generic_type_mismatch() {
    let union = AttributeType::union(vec![AttributeType::int(), AttributeType::bool()]);
    let value = Value::Concrete(ConcreteValue::String("neither".to_string()));
    let expected = TypeError::TypeMismatch {
        expected: union.type_name(),
        got: "String".to_string(),
    };

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(err, expected, "{path} changed scalar-union fallback");
    }
}

#[test]
fn tied_scalar_union_keeps_detailed_declaration_order_error() {
    fn reject_first(_: &Value) -> Result<(), String> {
        Err("first scalar alternative failed".to_string())
    }
    fn reject_second(_: &Value) -> Result<(), String> {
        Err("second scalar alternative failed".to_string())
    }
    let union = AttributeType::union(vec![
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::bare("FirstScalar")),
            None,
            None,
            legacy_validator(reject_first),
            None,
        ),
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::bare("SecondScalar")),
            None,
            None,
            legacy_validator(reject_second),
            None,
        ),
    ]);
    let value = Value::Concrete(ConcreteValue::String("value".to_string()));

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        assert_eq!(
            err,
            TypeError::ValidationFailed {
                message: "first scalar alternative failed".to_string()
            },
            "{path} must keep a useful scalar member error"
        );
    }
}

#[test]
fn union_unique_enum_match_preserves_structured_variant_payload() {
    let union = AttributeType::union(vec![
        AttributeType::int(),
        AttributeType::enum_(
            TypeIdentity::bare("Mode"),
            Some(vec!["fast".to_string(), "slow".to_string()]),
            vec![],
            None,
            None,
        ),
    ]);
    let value = Value::Concrete(ConcreteValue::enum_identifier("zzz".to_string()));

    for (path, err) in union_errors_from_public_paths(&union, &value) {
        let TypeError::InvalidEnumVariant {
            value,
            type_name,
            expected,
            ..
        } = err
        else {
            panic!("{path} discarded InvalidEnumVariant payload")
        };
        assert_eq!(value, "zzz");
        assert_eq!(type_name.as_deref(), Some("Mode"));
        assert_eq!(
            expected.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["fast", "slow"],
            "{path} changed the enum candidates consumed by the LSP quick-fix"
        );
    }
}

#[test]
fn canonicalize_union_preserves_pre_3754_declaration_order_for_structs() {
    let string_or_list = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::list(AttributeType::string()),
    ]);
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "Narrow".to_string(),
            vec![StructField::new("a", string_or_list).required()],
        ),
        AttributeType::struct_(
            "Wide".to_string(),
            vec![
                StructField::new("a", AttributeType::string()).required(),
                StructField::new("b", AttributeType::int()).required(),
            ],
        ),
    ]);
    let value = union_map(vec![
        ("a", Value::Concrete(ConcreteValue::String("s".to_string()))),
        ("b", Value::Concrete(ConcreteValue::Int(1))),
    ]);

    let canonicalized = Schema::flat(union).canonicalize(value);
    assert_eq!(
        canonicalized,
        union_map(vec![
            (
                "a",
                Value::Concrete(ConcreteValue::StringList(vec!["s".to_string()])),
            ),
            ("b", Value::Concrete(ConcreteValue::Int(1))),
        ]),
        "an error-diagnostic change must not move canonicalization from Narrow to Wide"
    );
}

#[test]
fn canonicalize_union_ref_preserves_pre_3754_declaration_order_for_structs() {
    let string_or_list = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::list(AttributeType::string()),
    ]);
    let schema = Schema {
        root: AttributeType::union(vec![
            AttributeType::ref_("Narrow"),
            AttributeType::ref_("Wide"),
        ]),
        defs: std::collections::BTreeMap::from([
            (
                "Narrow".to_string(),
                AttributeType::struct_(
                    "Narrow".to_string(),
                    vec![StructField::new("a", string_or_list).required()],
                ),
            ),
            (
                "Wide".to_string(),
                AttributeType::struct_(
                    "Wide".to_string(),
                    vec![
                        StructField::new("a", AttributeType::string()).required(),
                        StructField::new("b", AttributeType::int()).required(),
                    ],
                ),
            ),
        ]),
    };
    let value = union_map(vec![
        ("a", Value::Concrete(ConcreteValue::String("s".to_string()))),
        ("b", Value::Concrete(ConcreteValue::Int(1))),
    ]);

    let canonicalized = schema.canonicalize(value);
    assert_eq!(
        canonicalized,
        union_map(vec![
            (
                "a",
                Value::Concrete(ConcreteValue::StringList(vec!["s".to_string()])),
            ),
            ("b", Value::Concrete(ConcreteValue::Int(1))),
        ]),
        "tied Ref-to-Struct arms must keep declaration-order canonicalization"
    );
}

#[test]
fn union_string_vs_enum_picks_enum_error_for_string_input() {
    // Acceptance #2 from #2219: `Int | Enum` with an identifier
    // input that doesn't match any enum variant must surface the
    // `InvalidEnumVariant` error from the Enum member (so the
    // user sees `expected one of: fast, slow`), not a generic
    // `TypeMismatch` from the Int member.
    //
    // Updated for carina#2986 strict mode: the test input is an
    // `EnumIdentifier`, which is the legitimate identifier-shaped path
    // that lands in the enum-variant matcher. A `String` here would
    // short-circuit to `StringLiteralExpectedEnum`, which is a
    // different concern covered separately.
    let union_type = AttributeType::union(vec![
        AttributeType::int(),
        AttributeType::enum_(
            TypeIdentity::bare("Mode".to_string()),
            Some(vec!["fast".to_string(), "slow".to_string()]),
            vec![],
            None,
            None,
        ),
    ]);
    let err = union_type
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
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
        other => panic!("expected InvalidEnumVariant from the Enum member, got: {other:?}"),
    }
}

#[test]
fn union_list_vs_list_struct_picks_inner_struct_error() {
    // List<String> | List<Struct{...}>: input is a List of Maps where
    // one Map has an unknown field. The List<Struct> member's nested
    // `UnknownStructField { field: "typo", ... }` error should surface
    // — the user has to know that "typo" isn't a valid field on Item.
    let union_type = AttributeType::union(vec![
        AttributeType::list(AttributeType::string()),
        AttributeType::list(AttributeType::struct_(
            "Item".to_string(),
            vec![StructField::new("name", AttributeType::string())],
        )),
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
    let union_type = AttributeType::union(vec![
        AttributeType::int(),
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::bare("Arn")),
            None,
            None,
            legacy_validator(must_be_arn),
            None,
        ),
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
    let union_type = AttributeType::union(vec![AttributeType::int(), AttributeType::bool()]);
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
    let union_type = AttributeType::union(vec![
        AttributeType::refined_int_with_validator(
            Some(TypeIdentity::bare("PositiveInt")),
            None,
            legacy_validator(must_be_positive),
        ),
        AttributeType::bool(),
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
    let union_type = AttributeType::union(vec![
        AttributeType::struct_(
            "Principal".to_string(),
            vec![StructField::new("service", AttributeType::string())],
        ),
        AttributeType::string(),
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
    let attr = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("Region")),
        None,
        None,
        validator(move |v| match v {
            Value::Concrete(ConcreteValue::String(s)) if s == &allowed_region => Ok(()),
            Value::Concrete(ConcreteValue::String(s)) => Err(TypeError::ValidationFailed {
                message: format!("expected region {}, got {}", allowed_region, s),
            }),
            other => Err(TypeError::TypeMismatch {
                expected: "String".to_string(),
                got: other.type_name(),
            }),
        }),
        None,
    );
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
    let attr = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("Mode")),
        None,
        None,
        validator(|v| match v {
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
        None,
    );
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
fn schema_registry_routes_lookup_by_typestate() {
    // carina#3181: schema lookup routes by the resource typestate —
    // `get_for` for managed resources, `get_for_data_source` for data
    // sources.
    use crate::resource::{DataSource, Resource};

    let mut registry = SchemaRegistry::new();
    registry.insert("aws", ResourceSchema::new("s3.Bucket"));
    registry.insert("aws", ResourceSchema::new("s3.Bucket").as_data_source());

    let managed_res = Resource::with_provider("aws", "s3.Bucket", "new", None);
    let data_res = DataSource::with_provider("aws", "s3.Bucket", "existing", None);

    let m = registry
        .get_for(&managed_res)
        .expect("managed schema present");
    assert_eq!(m.kind, SchemaKind::Resource);

    let d = registry
        .get_for_data_source(&data_res)
        .expect("data source schema present");
    assert_eq!(d.kind, SchemaKind::DataSource);
}

#[test]
fn schema_registry_missing_returns_none() {
    let registry = SchemaRegistry::new();
    assert!(
        registry
            .get("aws", "s3.Bucket", SchemaKind::Resource)
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
    assert!(AttributeType::string().validate(&unknown).is_ok());
    assert!(AttributeType::int().validate(&unknown).is_ok());
    assert!(AttributeType::bool().validate(&unknown).is_ok());

    let upstream = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
        path: AccessPath::with_fields("net", "vpc", vec!["vpc_id".into()]),
    }));
    assert!(AttributeType::string().validate(&upstream).is_ok());
    assert!(AttributeType::int().validate(&upstream).is_ok());
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
    let custom_type = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("vpc_id")),
        None,
        None,
        always_fail,
        None,
    );

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
        crate::schema::empty_defs_for_schema_walks(),
        &mut errors,
    );
    assert!(
        errors.is_empty(),
        "Value::Deferred(DeferredValue::Unknown) must not invoke the custom validator, got: {errors:?}"
    );
}

fn resource_schema_with_self_cyclic_string_union() -> ResourceSchema {
    ResourceSchema::new("test.SelfCyclicUnion")
        .attribute(AttributeSchema::new("value", AttributeType::ref_("A")))
        .with_def(
            "A",
            AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::string()]),
        )
}

#[test]
fn resource_schema_validate_self_cyclic_union_with_valid_value_terminates() {
    let schema = resource_schema_with_self_cyclic_string_union();
    let attributes = HashMap::from([(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    )]);

    assert_eq!(schema.validate(&attributes), Ok(()));
}

#[test]
fn resource_schema_validate_self_cyclic_union_with_invalid_value_terminates() {
    let schema = resource_schema_with_self_cyclic_string_union();
    let attributes = HashMap::from([("value".to_string(), Value::Concrete(ConcreteValue::Int(1)))]);

    let errors = schema
        .validate(&attributes)
        .expect_err("Int must not satisfy the self-cyclic String union");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, TypeError::TypeMismatch { .. })),
        "the failing value must produce a defined type error, got: {errors:?}"
    );
}

#[test]
fn resource_schema_custom_lookup_resets_ref_path_after_list_descent() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let custom_string = AttributeType::refined_string_with_validator(
        Some(TypeIdentity::bare("cyclic_leaf")),
        None,
        None,
        validator(|_| Ok(())),
        None,
    );
    let schema = ResourceSchema::new("test.CyclicCustomLookup")
        .attribute(AttributeSchema::new("value", AttributeType::ref_("A")))
        .with_def(
            "A",
            AttributeType::union(vec![
                AttributeType::ref_("A"),
                custom_string,
                AttributeType::list(AttributeType::ref_("A")),
            ]),
        );
    let attributes = HashMap::from([(
        "value".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("x".to_string()),
        )])),
    )]);
    let calls = AtomicUsize::new(0);
    let lookup = |identity: &TypeIdentity, value: &Value| {
        if identity.kind != "cyclic_leaf" {
            return Ok(());
        }
        if matches!(value, Value::Concrete(ConcreteValue::String(_))) {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        } else {
            Err(TypeError::ValidationFailed {
                message: "cyclic_leaf expects a String".to_string(),
            })
        }
    };

    assert_eq!(
        schema.validate_with_origins_and_lookup(&attributes, &|_| false, &lookup),
        Ok(())
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "list-element descent must start a fresh Ref path and reach the custom leaf"
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
    let custom = |name: &str| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::bare(name)),
            None,
            None,
            validator(|_v: &Value| Ok(())),
            None,
        )
    };
    let union = AttributeType::union(vec![custom("ok_arm"), custom("fail_arm")]);

    let lookup = |id: &TypeIdentity, _v: &Value| -> Result<(), TypeError> {
        if id.kind == "ok_arm" {
            Ok(())
        } else {
            Err(TypeError::ValidationFailed {
                message: format!("{} rejects", id.kind),
            })
        }
    };

    let mut errors = Vec::new();
    walk_custom_lookup(
        &union,
        &Value::Concrete(ConcreteValue::String("anything".to_string())),
        "attr",
        &lookup,
        crate::schema::empty_defs_for_schema_walks(),
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
    let custom = |name: &str| {
        AttributeType::refined_string_with_validator(
            Some(TypeIdentity::bare(name)),
            None,
            None,
            validator(|_v: &Value| Ok(())),
            None,
        )
    };
    let union = AttributeType::union(vec![custom("a"), custom("b")]);

    let lookup = |id: &TypeIdentity, _v: &Value| -> Result<(), TypeError> {
        Err(TypeError::ValidationFailed {
            message: format!("{} rejects", id.kind),
        })
    };

    let mut errors = Vec::new();
    walk_custom_lookup(
        &union,
        &Value::Concrete(ConcreteValue::String("anything".to_string())),
        "attr",
        &lookup,
        crate::schema::empty_defs_for_schema_walks(),
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
    // Regression for awscc#217. WASM-plugin schemas arrive without
    // the schema-attached `Custom.validate` behavior; host-side
    // validation runs through `lookup` by semantic_name. Models that:
    // lookup approves `Ipv4Cidr` for
    // `"10.0.0.0/8"` and rejects `Ipv6Cidr`. The Union must surface
    // no errors — the previous loop would have pushed the IPv6
    // rejection through anyway. This is the awscc#217 reproduction.
    let cidr = AttributeType::union(vec![types::ipv4_cidr(), types::ipv6_cidr()]);

    let lookup = |id: &TypeIdentity, value: &Value| -> Result<(), TypeError> {
        let Value::Concrete(ConcreteValue::String(s)) = value else {
            return Ok(());
        };
        match id.kind.as_str() {
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
        crate::schema::empty_defs_for_schema_walks(),
        &mut errors,
    );
    assert!(
        errors.is_empty(),
        "Union<ipv4_cidr, ipv6_cidr> must accept `10.0.0.0/8`; got: {errors:?}"
    );
}

// =====================================================================
// carina#2831: `Enum.dsl_aliases` is the data form that survives
// the WASM-component boundary, replacing the closure-based `to_dsl`
// pointer that could not. Once aliases are populated, the validator
// must accept the DSL spellings, including in fully-qualified form
// (`awscc.<service>.<TypeName>.<dsl_spelling>`), and diagnostics must
// list the writable DSL spelling.
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
    let t = AttributeType::enum_(
        crate::schema::enum_identity("ObjectOwnership", Some("awscc.s3.Bucket")),
        Some(vec![
            "ObjectWriter".to_string(),
            "BucketOwnerPreferred".to_string(),
            "BucketOwnerEnforced".to_string(),
        ]),
        vec![
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
        None,
        None,
    );

    // Bare API spelling: REJECTED under strict mode (the DSL alias
    // rewrite invalidates the API spelling — users must type the DSL
    // form `bucket_owner_enforced`). Phase 4 of carina#2986 carries the
    // input as `EnumIdentifier` because the user wrote it as a bare
    // identifier, not a quoted string.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
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
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "bucket_owner_enforced".to_string()
        )))
        .is_ok()
    );
    // Fully-qualified API spelling: REJECTED under strict mode.
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.s3.Bucket.ObjectOwnership.BucketOwnerEnforced".to_string()
        )))
        .is_err()
    );
    // Fully-qualified DSL spelling: accepted (the awscc#199 case).
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.s3.Bucket.ObjectOwnership.bucket_owner_enforced".to_string()
        )))
        .is_ok()
    );

    // An unrelated value: rejected, listing only the DSL spellings
    // that `validate` accepts and users can type.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "garbage".to_string(),
        )))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("bucket_owner_enforced"),
        "diagnostic must list the DSL spelling, got: {msg}"
    );
    assert!(
        !msg.contains("BucketOwnerEnforced"),
        "diagnostic must not list the API spelling, got: {msg}"
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
    let t = AttributeType::enum_(
        crate::schema::enum_identity("TrafficType", Some("aws.ec2.FlowLog")),
        Some(vec![
            "ACCEPT".to_string(),
            "REJECT".to_string(),
            "ALL".to_string(),
        ]),
        vec![],
        None,
        None,
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "ALL".to_string()
        )))
        .is_ok(),
        "lax mode (empty dsl_aliases) accepts API canonical"
    );
}

#[test]
fn dsl_aliases_diagnostic_tags_alias_entries_distinct_from_canonical() {
    // The `expected` list carried by `TypeError::InvalidEnumVariant`
    // stores DSL spellings as canonical candidates for 1:1 alias
    // tables, so LSP code actions suggest writable identifiers.
    let t = AttributeType::enum_(
        crate::schema::enum_identity("VersioningStatus", Some("aws.s3.Bucket")),
        Some(vec!["Enabled".to_string(), "Suspended".to_string()]),
        vec![
            ("Enabled".to_string(), "enabled".to_string()),
            ("Suspended".to_string(), "suspended".to_string()),
        ],
        None,
        None,
    );
    // Use `EnumIdentifier` so the strict-mode validator reaches the
    // unknown-variant branch (the alias-tagging behavior we want to
    // pin). A `String` here would take the `StringLiteralExpectedEnum`
    // path which is covered by sibling tests.
    let err = t
        .validate(&Value::Concrete(ConcreteValue::enum_identifier(
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
    assert_eq!(canonical, vec!["enabled", "suspended"]);
    assert!(aliases.is_empty(), "1:1 aliases must collapse");
}

#[test]
fn dsl_aliases_empty_keeps_api_only_validation() {
    // An empty `dsl_aliases` is the wire-format default for older
    // providers and for enums whose API spelling already matches the
    // DSL spelling. Validation must continue to accept the API
    // spelling and nothing else.
    let t = AttributeType::enum_(
        crate::schema::enum_identity("Status", Some("test.r")),
        Some(vec!["active".to_string(), "inactive".to_string()]),
        vec![],
        None,
        None,
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "active".to_string()
        )))
        .is_ok()
    );
    assert!(
        t.validate(&Value::Concrete(ConcreteValue::enum_identifier(
            "nope".to_string()
        )))
        .is_err()
    );
}

// #2978 — collision between a `let` binding name and a `Enum`
// DSL alias. The parser resolves a bare identifier to the binding
// first (see parser/expressions/primary.rs:367-374), producing a
// `Value::Deferred(DeferredValue::BindingRef { binding: "vpc" })` for
// the attribute value. Without the dedicated check, the deferred value
// flows past `validate` and only surfaces later as a `${vpc}`-style
// error from the resolver, which is hard to diagnose. Pin the
// behavior here so a regression on the `validate` side is visible
// without round-tripping through plan execution.
mod enum_binding_collision {
    use super::*;
    use crate::resource::DeferredValue;

    fn flow_log_resource_type() -> AttributeType {
        // Mirrors the awscc.ec2.FlowLog.ResourceType enum that
        // surfaced the issue. The `"vpc"` alias is what collides with
        // a `let vpc = ...` binding name in real fixtures.
        AttributeType::enum_(
            crate::schema::enum_identity("ResourceType", Some("awscc.ec2.FlowLog")),
            Some(vec![
                "NetworkInterface".to_string(),
                "Subnet".to_string(),
                "VPC".to_string(),
            ]),
            vec![
                (
                    "NetworkInterface".to_string(),
                    "network_interface".to_string(),
                ),
                ("Subnet".to_string(), "subnet".to_string()),
                ("VPC".to_string(), "vpc".to_string()),
            ],
            None,
            None,
        )
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
    fn collision_check_only_fires_on_enum() {
        // Other AttributeType kinds (e.g., String) must not be
        // affected by the collision check — many of them legitimately
        // accept a `BindingRef`.
        let t = AttributeType::string();
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
        let value = Value::Concrete(ConcreteValue::enum_identifier("vpc".to_string()));
        assert!(t.validate(&value).is_ok());
    }

    #[test]
    fn fully_qualified_form_still_passes() {
        // `attribute = awscc.ec2.FlowLog.ResourceType.vpc` reaches the
        // validator as `ConcreteValue::EnumIdentifier` (carina#2986
        // Phase 3 parser routing).
        let t = flow_log_resource_type();
        let value = Value::Concrete(ConcreteValue::enum_identifier(
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
        let map = DslMap::new(&aliases, None);
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
        let map = DslMap::new(&aliases, None);
        assert_eq!(map.api_for("unknown"), "unknown");
    }

    #[test]
    fn aliases_returns_input_when_given_api_canonical() {
        // If the caller already has the API spelling, `api_for` is a
        // no-op: the alias table is `(api, dsl)`, so an API value
        // doesn't match any `dsl` side and falls through.
        let aliases = alias_table();
        let map = DslMap::new(&aliases, None);
        assert_eq!(map.api_for("Enabled"), "Enabled");
    }

    #[test]
    fn aliases_empty_table_is_identity() {
        let aliases: Vec<(String, String)> = vec![];
        let map = DslMap::new(&aliases, None);
        assert_eq!(map.api_for("anything"), "anything");
    }

    #[test]
    fn transform_reverses_lossless_dsl_spelling() {
        // HyphenToUnderscore is reversible enough for enum canonicalization:
        // provider-facing API values use hyphens while DSL spelling uses
        // underscores.
        let transform = crate::schema::DslTransform::HyphenToUnderscore;
        let map = DslMap::new(&[], Some(&transform));
        assert_eq!(map.api_for("ap_northeast_1"), "ap-northeast-1");
    }

    #[test]
    fn closure_none_returns_input_unchanged() {
        let map = DslMap::new(&[], None);
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
        let map = DslMap::new(&aliases, None);
        assert_eq!(map.api_for("bar"), "Foo");
    }
}

// carina#2996: `Map<Enum, V>` bare-identifier key acceptance.
fn condition_operator_map(value: AttributeType) -> AttributeType {
    AttributeType::map_with_key(
        AttributeType::enum_(
            crate::schema::enum_identity("ConditionOperator", Some("aws.iam.ConditionOperator")),
            Some(vec![
                "string_equals".to_string(),
                "string_not_equals".to_string(),
                "arn_like".to_string(),
            ]),
            vec![],
            None,
            None,
        ),
        value,
    )
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
fn validate_map_with_enum_key_accepts_bare_identifier_spelling() {
    let map_t = condition_operator_map(AttributeType::string());
    let val = map_value_with_one_key("string_equals");
    assert!(
        map_t.validate(&val).is_ok(),
        "bare-identifier map key for Map<Enum, V> must be accepted: {:?}",
        map_t.validate(&val)
    );
}

#[test]
fn validate_map_with_enum_key_accepts_dsl_alias_spelling() {
    // `IpProtocol` has a `("-1", "all")` alias: DSL must accept `all`
    // both at attribute-value position (already covered) and at
    // map-key position when used as `Map<Enum<IpProtocol>, V>`.
    let map_t = AttributeType::map_with_key(
        AttributeType::enum_(
            crate::schema::enum_identity("IpProtocol", Some("awscc.ec2.SecurityGroup")),
            Some(vec!["tcp".to_string(), "-1".to_string()]),
            vec![("-1".to_string(), "all".to_string())],
            None,
            None,
        ),
        AttributeType::string(),
    );
    let val = map_value_with_one_key("all");
    assert!(
        map_t.validate(&val).is_ok(),
        "DSL-alias map key spelling must be accepted: {:?}",
        map_t.validate(&val)
    );
}

#[test]
fn validate_map_with_enum_key_rejects_unknown_variant() {
    let map_t = condition_operator_map(AttributeType::string());
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

// awscc#251: persisted state files written before awscc#250 made IAM
// policy `version`/`effect` into `Enum` store these as plain JSON
// strings. On load they become `ConcreteValue::String`. The carina#2986
// Phase 4 strict validator then rejects them at the `Enum`
// position because it demands `ConcreteValue::EnumIdentifier`. The
// schema-aware state-migration lift in
// `crate::utils::lift_state_enum_leaves` walks loaded
// attributes against their schema and lifts recognized API-canonical /
// alias strings to `CanonicalEnum` so old state validates again.
#[test]
fn lift_state_enum_leaves_fixes_awscc251() {
    use crate::utils::lift_state_enum_leaves;
    use indexmap::IndexMap;

    let version_enum = AttributeType::enum_(
        crate::schema::enum_identity("Version", Some("aws.iam.PolicyDocument")),
        Some(vec!["2012-10-17".to_string(), "2008-10-17".to_string()]),
        vec![
            ("2012-10-17".to_string(), "2012_10_17".to_string()),
            ("2008-10-17".to_string(), "2008_10_17".to_string()),
        ],
        None,
        None,
    );
    let effect_enum = AttributeType::enum_(
        crate::schema::enum_identity("Effect", Some("aws.iam.PolicyDocument")),
        Some(vec!["Allow".to_string(), "Deny".to_string()]),
        vec![
            ("Allow".to_string(), "allow".to_string()),
            ("Deny".to_string(), "deny".to_string()),
        ],
        None,
        None,
    );
    let statement_struct = AttributeType::struct_(
        "Statement".to_string(),
        vec![StructField::new("effect", effect_enum)],
    );
    let policy_struct = AttributeType::struct_(
        "PolicyDocument".to_string(),
        vec![
            StructField::new("version", version_enum),
            StructField::new("statement", AttributeType::unordered_list(statement_struct)),
        ],
    );
    let schema = ResourceSchema::new("aws.iam.role_policy")
        .attribute(AttributeSchema::new("policy", policy_struct));

    // Attributes exactly as loaded from persisted state JSON: enum
    // positions hold `ConcreteValue::String`, not `EnumIdentifier`.
    let mut statement = IndexMap::new();
    statement.insert(
        "effect".to_string(),
        Value::Concrete(ConcreteValue::String("Allow".to_string())),
    );
    let mut policy = IndexMap::new();
    policy.insert(
        "version".to_string(),
        Value::Concrete(ConcreteValue::String("2012-10-17".to_string())),
    );
    policy.insert(
        "statement".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(statement),
        )])),
    );
    let mut attrs: HashMap<String, Value> = HashMap::new();
    attrs.insert(
        "policy".to_string(),
        Value::Concrete(ConcreteValue::Map(policy)),
    );

    // BEFORE the lift: strict validator rejects the loaded String.
    let before = schema.validate(&attrs);
    let errs = before.expect_err("loaded state String must fail strict Enum validation");
    fn has_string_literal_expected_enum(errs: &[TypeError]) -> bool {
        errs.iter().any(|e| match e {
            TypeError::StringLiteralExpectedEnum { .. } => true,
            TypeError::StructFieldError { inner, .. }
            | TypeError::ListItemError { inner, .. }
            | TypeError::MapValueError { inner, .. } => {
                has_string_literal_expected_enum(std::slice::from_ref(inner))
            }
            _ => false,
        })
    }
    assert!(
        has_string_literal_expected_enum(&errs),
        "expected StringLiteralExpectedEnum, got: {errs:?}"
    );

    // Apply the lift.
    lift_state_enum_leaves(&mut attrs, &schema);

    // AFTER the lift: validation passes.
    schema
        .validate(&attrs)
        .expect("lifted state must pass strict Enum validation");

    // The lifted values are CanonicalEnum in the provider API spelling.
    let Value::Concrete(ConcreteValue::Map(policy)) = &attrs["policy"] else {
        panic!("policy should be a Map");
    };
    assert!(matches!(
        &policy["version"],
        Value::Concrete(ConcreteValue::CanonicalEnum(c))
            if c.identity().to_string() == "aws.iam.PolicyDocument.Version"
                && c.api_value() == "2012-10-17"
    ));
    let Value::Concrete(ConcreteValue::List(stmts)) = &policy["statement"] else {
        panic!("statement should be a List");
    };
    let Value::Concrete(ConcreteValue::Map(stmt)) = &stmts[0] else {
        panic!("statement[0] should be a Map");
    };
    assert!(matches!(
        &stmt["effect"],
        Value::Concrete(ConcreteValue::CanonicalEnum(c))
            if c.identity().to_string() == "aws.iam.PolicyDocument.Effect"
                && c.api_value() == "Allow"
    ));
}

#[test]
fn lift_state_enums_is_idempotent_and_preserves_invalid() {
    use crate::utils::lift_state_enum_leaves;
    use indexmap::IndexMap;

    let version_enum = AttributeType::enum_(
        crate::schema::enum_identity("Version", Some("aws.iam.PolicyDocument")),
        Some(vec!["2012-10-17".to_string()]),
        vec![("2012-10-17".to_string(), "2012_10_17".to_string())],
        None,
        None,
    );
    let policy_struct = AttributeType::struct_(
        "PolicyDocument".to_string(),
        vec![StructField::new("version", version_enum)],
    );
    let schema = ResourceSchema::new("aws.iam.role_policy")
        .attribute(AttributeSchema::new("policy", policy_struct));

    // Case 1: already an EnumIdentifier (post-fix re-plan) is canonicalized.
    let mut already = IndexMap::new();
    already.insert(
        "version".to_string(),
        Value::Concrete(ConcreteValue::enum_identifier("2012_10_17".to_string())),
    );
    let mut attrs: HashMap<String, Value> = HashMap::new();
    attrs.insert(
        "policy".to_string(),
        Value::Concrete(ConcreteValue::Map(already.clone())),
    );
    lift_state_enum_leaves(&mut attrs, &schema);
    let Value::Concrete(ConcreteValue::Map(p)) = &attrs["policy"] else {
        panic!("map");
    };
    assert!(
        matches!(
            &p["version"],
            Value::Concrete(ConcreteValue::CanonicalEnum(c))
                if c.identity().to_string() == "aws.iam.PolicyDocument.Version"
                    && c.api_value() == "2012-10-17"
        ),
        "already-lifted EnumIdentifier must canonicalize to API value"
    );

    // Case 2: unrecognized string — left as String so the strict
    // validator still rejects genuinely-invalid persisted state rather
    // than masking it.
    let mut bad = IndexMap::new();
    bad.insert(
        "version".to_string(),
        Value::Concrete(ConcreteValue::String("1999-01-01".to_string())),
    );
    let mut attrs2: HashMap<String, Value> = HashMap::new();
    attrs2.insert(
        "policy".to_string(),
        Value::Concrete(ConcreteValue::Map(bad)),
    );
    lift_state_enum_leaves(&mut attrs2, &schema);
    let Value::Concrete(ConcreteValue::Map(p2)) = &attrs2["policy"] else {
        panic!("map");
    };
    assert_eq!(
        p2["version"],
        Value::Concrete(ConcreteValue::String("1999-01-01".to_string())),
        "unrecognized member must stay String (validator still rejects it)"
    );
    assert!(
        schema.validate(&attrs2).is_err(),
        "invalid persisted state must still fail validation, not be masked"
    );
}

#[test]
fn dynamic_enum_lift_raw_string_requires_transform_and_structural_dsl_member() {
    use crate::utils::lift_state_enum_leaves;

    fn az_schema() -> ResourceSchema {
        let zone_name = AttributeType::enum_(
            crate::schema::enum_identity("ZoneName", Some("aws.AvailabilityZone")),
            None,
            vec![],
            None,
            Some(crate::schema::DslTransform::HyphenToUnderscore),
        );
        ResourceSchema::new("aws.ec2.subnet")
            .attribute(AttributeSchema::new("availability_zone", zone_name))
    }

    fn lifted_value(raw: &str) -> Value {
        let schema = az_schema();
        let mut attrs = HashMap::from([(
            "availability_zone".to_string(),
            Value::Concrete(ConcreteValue::String(raw.to_string())),
        )]);
        lift_state_enum_leaves(&mut attrs, &schema);
        attrs.remove("availability_zone").unwrap()
    }

    assert_eq!(
        lifted_value("foo_bar_42"),
        Value::Concrete(ConcreteValue::String("foo_bar_42".to_string())),
        "raw strings whose transform is a no-op must stay String"
    );
    assert_eq!(
        lifted_value("Active"),
        Value::Concrete(ConcreteValue::String("Active".to_string())),
        "uppercase raw strings must stay String"
    );
    assert!(
        matches!(
            lifted_value("ap-northeast-1z"),
            Value::Concrete(ConcreteValue::CanonicalEnum(c))
                if c.identity().to_string() == "aws.AvailabilityZone.ZoneName"
                    && c.api_value() == "ap-northeast-1z"
        ),
        "structural API-form dynamic enum strings must lift"
    );
    assert_eq!(
        lifted_value("123_abc"),
        Value::Concrete(ConcreteValue::String("123_abc".to_string())),
        "raw strings that already look DSL-like but start with a digit must stay String"
    );
}

#[test]
fn dsl_map_is_empty_means_no_aliases_and_no_transform() {
    let empty_aliases: Vec<(String, String)> = Vec::new();
    assert!(
        DslMap::new(&empty_aliases, None).is_empty(),
        "no aliases and no transform means no rewrite machinery"
    );
    assert!(
        !DslMap::new(
            &empty_aliases,
            Some(&crate::schema::DslTransform::HyphenToUnderscore)
        )
        .is_empty(),
        "a dynamic transform counts as rewrite machinery even when aliases are empty"
    );
    let aliases = vec![("Allow".to_string(), "allow".to_string())];
    assert!(
        !DslMap::new(&aliases, None).is_empty(),
        "alias-table entries count as rewrite machinery"
    );
}

// carina#3080 type-safety guard: Union selection and Union validation
// must use the same structural judgement. For the shape-disjoint carina#3080
// schema that means both identify the same unique member. These tests pin the
// property that makes the `None` (identity) arm unreachable, so a future
// judgement/schema change that breaks selection fails loudly instead of
// silently skipping the canonicalization fold and re-introducing the phantom
// diff.

fn principal_union_schema() -> AttributeType {
    AttributeType::union(vec![
        AttributeType::struct_(
            "PrincipalStruct".to_string(),
            vec![StructField::new(
                "service",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            )],
        ),
        AttributeType::string(),
    ])
}

fn select_union_member_for_test<'a>(
    union: &'a AttributeType,
    value: &Value,
) -> Option<ResolvedUnionMember<'a>> {
    let members = union_members_with_defs(union, empty_defs_for_schema_walks())
        .expect("test fixture must be a Union");
    let mut visited_refs = Vec::new();
    members.select_on_path(value, &mut visited_refs)
}

#[test]
fn select_union_member_retains_only_the_chosen_member_ref_chain() {
    let root = AttributeType::ref_("Outer");
    let defs = std::collections::BTreeMap::from([
        (
            "Outer".to_string(),
            AttributeType::union(vec![AttributeType::ref_("Chosen"), AttributeType::int()]),
        ),
        ("Chosen".to_string(), AttributeType::string()),
    ]);
    let members = union_members_with_defs(&root, &defs).expect("Outer must resolve to a Union");
    let value = Value::Concrete(ConcreteValue::String("x".to_string()));
    let mut visited_refs = vec!["Caller"];

    let selected = members
        .select_on_path(&value, &mut visited_refs)
        .expect("String value must select Chosen");
    assert!(matches!(
        selected.as_attr().kind(),
        AttrTypeKind::String { .. }
    ));
    assert_eq!(visited_refs, vec!["Caller", "Chosen"]);
}

#[test]
fn select_union_member_picks_struct_for_map_value() {
    let union = principal_union_schema();
    let mut map = IndexMap::new();
    map.insert(
        "service".to_string(),
        Value::Concrete(ConcreteValue::String(
            "cloudfront.amazonaws.com".to_string(),
        )),
    );
    let v = Value::Concrete(ConcreteValue::Map(map));
    let chosen = select_union_member_for_test(&union, &v).expect("a Map must select a member");
    assert!(
        matches!(chosen.as_attr().kind(), AttrTypeKind::Struct { .. }),
        "a Map value must select the Struct member, got {chosen:?}"
    );
}

/// The negative the design calls out: a `Map` value must NEVER select
/// the `String` member of `Union[Struct, String]`, regardless of
/// declaration order.
#[test]
fn select_union_member_map_never_picks_string_member() {
    let mut map = IndexMap::new();
    map.insert(
        "service".to_string(),
        Value::Concrete(ConcreteValue::String("x".to_string())),
    );
    let v = Value::Concrete(ConcreteValue::Map(map));

    // Struct-before-String (the real `string_or_principal_struct` order).
    let a = principal_union_schema();
    assert!(matches!(
        select_union_member_for_test(&a, &v).map(|m| m.as_attr().kind()),
        Some(AttrTypeKind::Struct { .. })
    ));

    // String-before-Struct: still must not pick String for a Map.
    let b = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::struct_(
            "PrincipalStruct".to_string(),
            vec![StructField::new("service", AttributeType::string())],
        ),
    ]);
    assert!(
        matches!(
            select_union_member_for_test(&b, &v).map(|m| m.as_attr().kind()),
            Some(AttrTypeKind::Struct { .. })
        ),
        "a Map must select Struct even when String is declared first"
    );
}

/// Scalar string under `string_or_list_of_strings` selects a member
/// (so the nested fold is reachable, never the `None` identity arm).
#[test]
fn select_union_member_scalar_selects_string_or_list() {
    let union = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::list(AttributeType::string()),
    ]);
    let v = Value::Concrete(ConcreteValue::String(
        "cloudfront.amazonaws.com".to_string(),
    ));
    assert!(
        select_union_member_for_test(&union, &v).is_some(),
        "a scalar must select the String member, not fall to None"
    );
    let list = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::String("cloudfront.amazonaws.com".to_string()),
    )]));
    assert!(
        select_union_member_for_test(&union, &list).is_some(),
        "a singleton list must select the List member, not fall to None"
    );
}

/// No member shares the value's shape → `None` (identity at call site).
#[test]
fn select_union_member_no_match_is_none() {
    let union = AttributeType::union(vec![AttributeType::int(), AttributeType::bool()]);
    let v = Value::Concrete(ConcreteValue::String("not-an-int".to_string()));
    assert!(select_union_member_for_test(&union, &v).is_none());
}

/// Deferred values have no concrete shape → `None` (the canonicalizer
/// leaves them for a later post-resolution pass).
#[test]
fn select_union_member_deferred_value_is_none() {
    let union = principal_union_schema();
    let v = Value::Deferred(DeferredValue::BindingRef {
        binding: "some_ref".to_string(),
    });
    assert!(select_union_member_for_test(&union, &v).is_none());
}

// ---------------------------------------------------------------------------
// carina#3340: AttributeType::Ref + Schema for cyclic struct definitions.
//
// The CFN schema for WAFv2 WebACL has a cyclic definition graph:
//     Statement -> AndStatement.Statements: List<Statement> -> ...
// Modelled as:
//     defs["Statement"]    = Struct { fields: [and_statement: Ref("AndStatement"), ...] }
//     defs["AndStatement"] = Struct { fields: [statements: List(Ref("Statement"))] }
// The root attribute references into `defs` via `Ref`.
// ---------------------------------------------------------------------------

fn cyclic_statement_schema() -> Schema {
    let mut defs = std::collections::BTreeMap::new();
    defs.insert(
        "Statement".to_string(),
        AttributeType::struct_(
            "Statement".to_string(),
            vec![StructField::new(
                "and_statement",
                AttributeType::ref_("AndStatement".to_string()),
            )],
        ),
    );
    defs.insert(
        "AndStatement".to_string(),
        AttributeType::struct_(
            "AndStatement".to_string(),
            vec![StructField::new(
                "statements",
                AttributeType::list(AttributeType::ref_("Statement".to_string())),
            )],
        ),
    );
    Schema {
        root: AttributeType::ref_("Statement".to_string()),
        defs,
    }
}

#[test]
fn schema_resolve_returns_defined_type() {
    let schema = cyclic_statement_schema();
    let resolved = schema.resolve("Statement").expect("Statement defined");
    assert!(matches!(resolved.kind(), AttrTypeKind::Struct { name, .. } if name == "Statement"));
    assert!(schema.resolve("NoSuchThing").is_none());
}

#[test]
fn schema_validate_well_formed_nested_statement_succeeds() {
    let schema = cyclic_statement_schema();

    let inner_statement = Value::Concrete(ConcreteValue::Map({
        let mut m = indexmap::IndexMap::new();
        m.insert(
            "and_statement".to_string(),
            Value::Concrete(ConcreteValue::Map({
                let mut m2 = indexmap::IndexMap::new();
                m2.insert(
                    "statements".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![])),
                );
                m2
            })),
        );
        m
    }));

    let outer = Value::Concrete(ConcreteValue::Map({
        let mut m = indexmap::IndexMap::new();
        m.insert(
            "and_statement".to_string(),
            Value::Concrete(ConcreteValue::Map({
                let mut m2 = indexmap::IndexMap::new();
                m2.insert(
                    "statements".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![inner_statement])),
                );
                m2
            })),
        );
        m
    }));

    let result = schema.validate(&outer);
    assert!(
        result.is_ok(),
        "well-formed nested statement should validate: {result:?}"
    );
}

#[test]
fn schema_validate_malformed_nested_statement_fails() {
    let schema = cyclic_statement_schema();

    // and_statement.statements should be a list of Statement (= Map),
    // but we put an Int instead.
    let bad = Value::Concrete(ConcreteValue::Map({
        let mut m = indexmap::IndexMap::new();
        m.insert(
            "and_statement".to_string(),
            Value::Concrete(ConcreteValue::Map({
                let mut m2 = indexmap::IndexMap::new();
                m2.insert(
                    "statements".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::Int(42),
                    )])),
                );
                m2
            })),
        );
        m
    }));

    let result = schema.validate(&bad);
    assert!(
        result.is_err(),
        "Int where Statement was expected should be rejected"
    );
}

#[test]
fn attribute_type_validate_on_ref_returns_error_without_schema() {
    let t = AttributeType::ref_("Statement".to_string());
    let v = Value::Concrete(ConcreteValue::String("anything".to_string()));
    let result = t.validate(&v);
    assert!(
        result.is_err(),
        "AttributeType::Ref cannot self-validate without a Schema"
    );
}

#[test]
fn raw_shape_preserves_ref_at_top() {
    // RawShape::Ref is the carina#3349 follow-up: transport-site
    // callers (WASM plugin↔host serializers) must round-trip Ref
    // without resolving it, because resolving against the local
    // `defs` would either infinite-loop on cyclic schemas
    // (WAFv2 WebACL.Statement) or flatten the structure the
    // receiver needs to rebuild from `defs`.
    let t = AttributeType::ref_("Statement");
    match t.raw_shape() {
        RawShape::Ref(name) => assert_eq!(name, "Statement"),
        other => panic!("expected RawShape::Ref(\"Statement\"), got {other:?}"),
    }
}

#[test]
fn raw_shape_passes_through_non_ref_variants() {
    // Sanity check that non-Ref variants still project correctly.
    assert!(matches!(
        AttributeType::string().raw_shape(),
        RawShape::String { .. }
    ));
    assert!(matches!(
        AttributeType::int().raw_shape(),
        RawShape::Int { .. }
    ));
    assert!(matches!(AttributeType::bool().raw_shape(), RawShape::Bool));
    match AttributeType::list(AttributeType::string()).raw_shape() {
        RawShape::List { ordered, .. } => assert!(ordered),
        other => panic!("expected RawShape::List, got {other:?}"),
    }
    match AttributeType::unordered_list(AttributeType::string()).raw_shape() {
        RawShape::List { ordered, .. } => assert!(!ordered),
        other => panic!("expected RawShape::List(unordered), got {other:?}"),
    }
}
