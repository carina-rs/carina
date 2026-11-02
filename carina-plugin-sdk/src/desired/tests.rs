async fn with_type_checks<T>(
    producer: impl for<'s> AsyncFnOnce(
        &'s carina_core::validation::deferred_types::TypeCheckSession<'s>,
    ) -> T,
) -> T {
    let mut diagnostics = Vec::new();
    let output = carina_core::validation::deferred_types::TypeCheckSession::run_async(
        &carina_core::parser::ProviderContext::default(),
        producer,
    )
    .await
    .publish(&mut diagnostics);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    output
}

use super::*;
use carina_core::protocol_convert::ProtocolConvertError;
use carina_core::resource::{ApplyTimeUnknown, ConcreteValue, DeferredValue, Value};
use std::error::Error;

fn native_cause(error: &NativeProviderError) -> &DesiredNormalizationError {
    error.detail().cause().unwrap().downcast_ref().unwrap()
}

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}
fn secret(value: &str) -> Value {
    Value::Deferred(DeferredValue::Secret(Box::new(string(value))))
}
fn reference() -> Value {
    Value::Deferred(DeferredValue::ResourceRef {
        path: serde_json::from_value(serde_json::json!({"binding":"peer", "attribute":"id"}))
            .unwrap(),
    })
}
fn unknown() -> Value {
    Value::Deferred(DeferredValue::Unknown(
        ApplyTimeUnknown::UpstreamAllExports {
            binding: serde_json::from_value(serde_json::json!("peer")).unwrap(),
        },
    ))
}
fn map(values: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        values
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}
fn list(values: Vec<Value>) -> Value {
    Value::Concrete(ConcreteValue::List(values))
}
fn scalar(value: &str) -> DesiredValue {
    DesiredValue::Known(DesiredScalar::String(value.into()))
}

fn normalize(view: &mut DesiredView<'_>) {
    if view.is_resource("aws", "acm.Certificate") {
        let apex = match view.attribute("domain_name") {
            Some(DesiredValue::Known(value)) => value
                .as_str()
                .map(|value| value.trim_end_matches('.').to_ascii_lowercase()),
            _ => None,
        };
        if let Some(DesiredValueMut::List(names)) = view.attribute_mut("subject_alternative_names")
        {
            names.retain_known(|value| {
                if let DesiredScalar::String(name) = value {
                    *name = name
                        .expose_plaintext()
                        .trim_end_matches('.')
                        .to_ascii_lowercase()
                        .into();
                    return apex.as_deref() != Some(name.expose_plaintext());
                }
                true
            });
        }
    }
    if view.is_resource("aws", "s3.BucketReplicationConfiguration")
        && let Some(DesiredValueMut::List(rules)) = view.attribute_mut("rules")
    {
        for rule in rules.iter_mut() {
            if let DesiredValueMut::Map(rule) = rule
                && rule.get("delete_marker_replication").is_none()
            {
                let mut default = DesiredMap::new();
                default.insert_absent("status", scalar("Disabled")).unwrap();
                rule.insert_absent("delete_marker_replication", DesiredValue::Map(default))
                    .unwrap();
            }
        }
    }
    if view.is_resource("aws", "route53.RecordSet") {
        if let Some(DesiredValueMut::Known(DesiredScalar::String(name))) =
            view.attribute_mut("name")
            && let Some(stripped) = name.expose_plaintext().strip_suffix('.')
        {
            *name = stripped.to_string().into();
        }
        if let Some(DesiredValueMut::Map(alias)) = view.attribute_mut("alias_target")
            && let Some(DesiredValueMut::Known(DesiredScalar::String(name))) =
                alias.get_mut("dns_name")
            && let Some(stripped) = name.expose_plaintext().strip_suffix('.')
        {
            *name = stripped.to_string().into();
        }
    }
    if view.is_resource("aws", "ec2.Subnet")
        && let Some(DesiredValueMut::Known(DesiredScalar::String(value))) =
            view.attribute_mut("ipv6_cidr_block")
    {
        let (address, prefix) = value.expose_plaintext().rsplit_once('/').unwrap();
        let address: std::net::Ipv6Addr = address.parse().unwrap();
        *value = format!("{address}/{prefix}").into();
    }
}

fn assert_structure(left: &Value, right: &Value) {
    match (left, right) {
        (
            Value::Deferred(DeferredValue::Secret(left)),
            Value::Deferred(DeferredValue::Secret(right)),
        ) => assert_structure(left, right),
        (
            Value::Deferred(DeferredValue::Unknown(left)),
            Value::Deferred(DeferredValue::Unknown(right)),
        ) => assert_eq!(format!("{left:?}"), format!("{right:?}")),
        (
            Value::Concrete(ConcreteValue::List(left)),
            Value::Concrete(ConcreteValue::List(right)),
        ) => {
            assert_eq!(left.len(), right.len());
            for (left, right) in left.iter().zip(right) {
                assert_structure(left, right);
            }
        }
        (Value::Concrete(ConcreteValue::Map(left)), Value::Concrete(ConcreteValue::Map(right))) => {
            assert_eq!(left.len(), right.len());
            for (key, value) in left {
                assert_structure(value, &right[key]);
            }
        }
        _ => assert_eq!(left, right),
    }
}

fn roundtrip(resource: &mut NormalizationResource) {
    for (name, value) in &mut resource.attributes {
        if matches!(value, Wire::List(_) | Wire::Map(_)) {
            *value = Wire::from_json(
                &value.to_json(&resource.id, name).unwrap(),
                &resource.id,
                name,
            )
            .unwrap();
        }
    }
}

fn both(original: Resource, action: impl Fn(&mut DesiredView<'_>)) -> (Resource, Resource) {
    let batch = NormalizationBatch::new(std::slice::from_ref(&original)).unwrap();
    let mut input = batch.resources()[0].clone();
    roundtrip(&mut input);
    let mut guest = DesiredResource::from_wire(input)
        .into_normalization()
        .unwrap();
    action(&mut guest.view());
    let mut normalized = guest.finish().unwrap();
    roundtrip(&mut normalized.resource);
    let mut host = original.clone();
    host.attributes = batch.finish(vec![normalized.resource]).unwrap().remove(0);
    let mut native = original;
    let mut owner = NativeDesiredNormalization::from_core(&mut native).unwrap();
    action(&mut owner.view());
    owner.finish().unwrap();
    assert_eq!(host.id, native.id);
    assert_eq!(
        host.attributes.keys().collect::<Vec<_>>(),
        native.attributes.keys().collect::<Vec<_>>()
    );
    for (name, value) in &native.attributes {
        assert_structure(value, &host.attributes[name]);
    }
    (native, host)
}

#[test]
fn mixed_acm_sans_normalize_known_names_and_remove_known_apex() {
    let original = Resource::with_provider("aws", "acm.Certificate", "example", None)
        .with_attribute("domain_name", string("example.com"))
        .with_attribute(
            "subject_alternative_names",
            list(vec![
                string("EXAMPLE.COM."),
                string("WWW.EXAMPLE.COM."),
                reference(),
                secret("private.example.com"),
                unknown(),
            ]),
        );
    let (native, _) = both(original, normalize);
    let expected = list(vec![
        string("www.example.com"),
        reference(),
        secret("private.example.com"),
        unknown(),
    ]);
    assert_structure(&native.attributes["subject_alternative_names"], &expected);
}

#[test]
fn mixed_replication_rules_default_known_rules_beside_deferred_leaves() {
    let original =
        Resource::with_provider("aws", "s3.BucketReplicationConfiguration", "example", None)
            .with_attribute(
                "rules",
                list(vec![
                    map([
                        ("id", secret("private-rule")),
                        ("destination", map([("bucket", reference())])),
                    ]),
                    reference(),
                    secret("opaque-rule"),
                    map([
                        ("id", string("second")),
                        ("delete_marker_replication", unknown()),
                    ]),
                ]),
            );
    let (native, _) = both(original, normalize);
    let expected = list(vec![
        map([
            ("id", secret("private-rule")),
            ("destination", map([("bucket", reference())])),
            (
                "delete_marker_replication",
                map([("status", string("Disabled"))]),
            ),
        ]),
        reference(),
        secret("opaque-rule"),
        map([
            ("id", string("second")),
            ("delete_marker_replication", unknown()),
        ]),
    ]);
    assert_structure(&native.attributes["rules"], &expected);
}

#[test]
fn mixed_route53_alias_normalizes_known_dns_beside_secret_and_ref() {
    let original = Resource::with_provider("aws", "route53.RecordSet", "example", None)
        .with_attribute("name", string("example.com."))
        .with_attribute(
            "alias_target",
            map([
                ("dns_name", string("target.example.com.")),
                ("hosted_zone_id", reference()),
                ("evaluate_target_health", secret("private")),
                ("unknown", unknown()),
            ]),
        );
    let (native, _) = both(original, normalize);
    assert_structure(&native.attributes["name"], &string("example.com"));
    assert_structure(
        &native.attributes["alias_target"],
        &map([
            ("dns_name", string("target.example.com")),
            ("hosted_zone_id", reference()),
            ("evaluate_target_health", secret("private")),
            ("unknown", unknown()),
        ]),
    );
}

#[test]
fn subnet_known_scalar_edits_and_top_level_deferred_queries_agree() {
    let original = Resource::with_provider("aws", "ec2.Subnet", "example", None)
        .with_attribute(
            "ipv6_cidr_block",
            string("2001:0DB8:0000:0000:0000:0000:0000:0000/64"),
        )
        .with_attribute("secret", secret("private"))
        .with_attribute("ref", reference())
        .with_attribute("unknown", unknown());
    let (native, _) = both(original, |view| {
        assert!(view.attribute("absent").is_none());
        for name in ["secret", "ref", "unknown"] {
            assert!(matches!(
                view.attribute(name),
                Some(DesiredValue::Deferred(_))
            ));
            assert!(matches!(
                view.attribute_mut(name),
                Some(DesiredValueMut::Deferred)
            ));
        }
        normalize(view);
    });
    assert_structure(
        &native.attributes["ipv6_cidr_block"],
        &string("2001:db8::/64"),
    );
}

fn validate(inputs: DesiredInputs<'_>) -> bool {
    // NAT mode/subnet facts and SQS required-together/FIFO facts use identical reads.
    if inputs.is_resource("aws", "ec2.NatGateway") {
        let mode = match inputs.attribute("availability_mode") {
            None => Some("zonal"),
            Some(DesiredValue::Known(value)) => value.as_str(),
            _ => None,
        };
        return match mode {
            Some("regional") => {
                !matches!(inputs.attribute("subnet_id"),Some(DesiredValue::Known(value)) if value.as_str().is_some())
            }
            Some("zonal") => inputs.attribute("subnet_id").is_some(),
            _ => true,
        };
    }
    let known = |name| matches!(inputs.attribute(name), Some(DesiredValue::Known(_)));
    if known("kms_data_key_reuse_period_seconds") && inputs.attribute("kms_master_key_id").is_none()
    {
        return false;
    }
    let standard = matches!(
        inputs.attribute("fifo_queue"),
        None | Some(DesiredValue::Known(DesiredScalar::Bool(false)))
    );
    !standard
        || !(matches!(
            inputs.attribute("content_based_deduplication"),
            Some(DesiredValue::Known(DesiredScalar::Bool(true)))
        ) || known("deduplication_scope")
            || known("fifo_throughput_limit"))
}

#[test]
fn nat_and_sqs_validation_distinguishes_known_absent_and_deferred() {
    for (kind, name, value, expected) in [
        ("ec2.NatGateway", "subnet_id", reference(), true),
        (
            "sqs.Queue",
            "kms_data_key_reuse_period_seconds",
            unknown(),
            true,
        ),
        (
            "sqs.Queue",
            "kms_data_key_reuse_period_seconds",
            Value::Concrete(ConcreteValue::Int(60)),
            false,
        ),
        ("sqs.Queue", "deduplication_scope", secret("private"), true),
        (
            "sqs.Queue",
            "deduplication_scope",
            string("messageGroup"),
            false,
        ),
        (
            "sqs.Queue",
            "content_based_deduplication",
            Value::Concrete(ConcreteValue::Bool(true)),
            false,
        ),
    ] {
        let original =
            Resource::with_provider("aws", kind, "example", None).with_attribute(name, value);
        let mut native = original.clone();
        let normalization = NativeDesiredNormalization::from_core(&mut native).unwrap();
        assert_eq!(validate(normalization.inputs()), expected);
        both(original, |view| {
            assert_eq!(validate(view.inputs()), expected)
        });
    }
}

#[test]
fn nat_validation_reads_native_enum_and_guest_string_identically() {
    use carina_core::resource::EnumValueResolver;
    use carina_core::schema::{CompiledAttributeType, TypeIdentity};
    let mode_type = CompiledAttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["ec2", "NatGateway"], "AvailabilityMode"),
        Some(vec!["regional".into(), "zonal".into()]),
        Vec::new(),
        None,
        None,
    );
    let mode = EnumValueResolver::new(&mode_type)
        .resolve_state_text("regional")
        .unwrap();
    for (subnet, expected) in [
        (string("subnet-123"), false),
        (reference(), true),
        (secret("private-subnet"), true),
    ] {
        let resource = Resource::with_provider("aws", "ec2.NatGateway", "enum-mode", None)
            .with_attribute(
                "availability_mode",
                Value::Concrete(ConcreteValue::CanonicalEnum(mode.clone())),
            )
            .with_attribute("subnet_id", subnet);
        both(resource, |view| {
            assert_eq!(validate(view.inputs()), expected)
        });
    }
}

#[test]
fn top_level_defaults_only_insert_into_absent_attributes_on_both_routes() {
    let original = Resource::new("test", "defaults")
        .with_attribute("known", string("kept"))
        .with_attribute("pending", secret("private"));
    let (native, _) = both(original, |view| {
        view.insert_absent("default", scalar("inserted")).unwrap();
        for name in ["known", "pending", "default"] {
            assert_eq!(
                view.insert_absent(name, scalar("replacement")),
                Err(DesiredEditError::Occupied { name: name.into() })
            );
        }
    });
    assert_structure(&native.attributes["default"], &string("inserted"));
    assert_structure(&native.attributes["known"], &string("kept"));
}

#[test]
fn list_defaults_appends_and_known_key_removal_work_on_both_routes() {
    let original = Resource::new("test", "edits")
        .with_attribute("obsolete", string("removed"))
        .with_attribute("items", list(vec![string("first")]))
        .with_attribute("settings", map([("known", string("removed"))]));
    let (native, _) = both(original, |view| {
        let mut defaults = DesiredList::new();
        defaults.push(scalar("default"));
        view.insert_absent("defaults", DesiredValue::List(defaults))
            .unwrap();
        view.insert_absent(
            "from_vec",
            DesiredValue::List(DesiredList::from(vec![scalar("from-vec")])),
        )
        .unwrap();
        let Some(DesiredValueMut::List(items)) = view.attribute_mut("items") else {
            panic!()
        };
        items.push(scalar("appended"));
        assert!(matches!(
            view.remove("obsolete"),
            Some(DesiredValue::Known(_))
        ));
        assert!(view.remove("absent").is_none());
        let Some(DesiredValueMut::Map(settings)) = view.attribute_mut("settings") else {
            panic!()
        };
        assert!(matches!(
            settings.remove("known"),
            Some(DesiredValue::Known(_))
        ));
        assert!(settings.remove("absent").is_none());
    });
    assert_structure(
        &native.attributes["defaults"],
        &list(vec![string("default")]),
    );
    assert_structure(
        &native.attributes["from_vec"],
        &list(vec![string("from-vec")]),
    );
    assert_structure(
        &native.attributes["items"],
        &list(vec![string("first"), string("appended")]),
    );
    assert_structure(&native.attributes["settings"], &map([]));
    assert!(!native.attributes.contains_key("obsolete"));
}

#[test]
fn dropping_removed_deferred_values_is_rejected_on_both_routes() {
    let original = Resource::new("test", "remove")
        .with_attribute("settings", map([("pending", secret("private"))]));
    for top_level in [false, true] {
        let remove = |view: &mut DesiredView<'_>| {
            if top_level {
                drop(view.remove("settings"));
            } else {
                let Some(DesiredValueMut::Map(settings)) = view.attribute_mut("settings") else {
                    panic!()
                };
                drop(settings.remove("pending"));
            }
        };
        let batch = NormalizationBatch::new(std::slice::from_ref(&original)).unwrap();
        let mut guest = DesiredResource::from_wire(batch.resources()[0].clone())
            .into_normalization()
            .unwrap();
        remove(&mut guest.view());
        assert!(matches!(
            guest.finish().unwrap_err().kind,
            Kind::MissingHandle { .. }
        ));
        let mut native = original.clone();
        let mut owner = NativeDesiredNormalization::from_core(&mut native).unwrap();
        remove(&mut owner.view());
        assert!(matches!(
            native_cause(&owner.finish().unwrap_err()).kind,
            Kind::MissingHandle { .. }
        ));
        assert_structure(
            &native.attributes["settings"],
            &original.attributes["settings"],
        );
    }
}

#[test]
fn removed_deferred_values_can_move_within_their_attribute_on_both_routes() {
    let original = Resource::new("test", "move")
        .with_attribute("settings", map([("pending", secret("private"))]));
    let (native, _) = both(original, |view| {
        let value = view.remove("settings").unwrap();
        view.insert_absent("settings", value).unwrap();
        let Some(DesiredValueMut::Map(settings)) = view.attribute_mut("settings") else {
            panic!()
        };
        let pending = settings.remove("pending").unwrap();
        settings
            .insert_absent(
                "moved",
                DesiredValue::List(DesiredList::from(vec![pending])),
            )
            .unwrap();
    });
    assert_structure(
        &native.attributes["settings"],
        &map([("moved", list(vec![secret("private")]))]),
    );
}

#[test]
fn enum_and_duration_have_identical_wire_scalar_kinds_on_both_routes() {
    use carina_core::resource::EnumValueResolver;
    use carina_core::schema::{CompiledAttributeType, TypeIdentity};
    let ty = CompiledAttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["ec2", "NatGateway"], "AvailabilityMode"),
        Some(vec!["regional".into()]),
        Vec::new(),
        None,
        None,
    );
    let mode = EnumValueResolver::new(&ty)
        .resolve_state_text("regional")
        .unwrap();
    let resource = Resource::new("test", "scalar-kinds")
        .with_attribute("mode", Value::Concrete(ConcreteValue::CanonicalEnum(mode)))
        .with_attribute(
            "timeout",
            Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(42))),
        );
    let batch = NormalizationBatch::new(std::slice::from_ref(&resource)).unwrap();
    let host = DesiredResource::from_wire(batch.resources()[0].clone())
        .into_normalization()
        .unwrap();
    let mut native_resource = resource.clone();
    let native_normalization = NativeDesiredNormalization::from_core(&mut native_resource).unwrap();
    let native = native_normalization.inputs();
    for (name, expected) in [("mode", "string:regional"), ("timeout", "int:42")] {
        let describe = |value: Option<&DesiredValue>| match value {
            Some(DesiredValue::Known(DesiredScalar::String(value))) => {
                format!("string:{}", value.expose_plaintext())
            }
            Some(DesiredValue::Known(DesiredScalar::Int(value))) => format!("int:{value}"),
            other => format!("{other:?}"),
        };
        assert_eq!(describe(host.attributes.get(name)), expected);
        assert_eq!(
            describe(native.attribute(name)),
            expected,
            "native scalar for {name}"
        );
    }
}

#[test]
fn deferred_reordering_is_allowed_but_dropping_or_foreign_movement_is_atomic() {
    let mut original = Resource::new("test", "example").with_attribute(
        "items",
        list(vec![reference(), secret("private"), string("known")]),
    );
    both(original.clone(), |view| {
        let Some(DesiredValueMut::List(items)) = view.attribute_mut("items") else {
            panic!()
        };
        items.move_within(0, 2).unwrap();
    });
    for foreign in [false, true] {
        let snapshot = original.clone();
        let mut owner = NativeDesiredNormalization::from_core(&mut original).unwrap();
        let node = owner.inner.attributes.entries.remove("items").unwrap();
        if foreign {
            owner.inner.attributes.entries.insert("other".into(), node);
        }
        let error = owner.finish().unwrap_err();
        assert!(if foreign {
            matches!(native_cause(&error).kind, Kind::ForeignHandle { .. })
        } else {
            matches!(native_cause(&error).kind, Kind::MissingHandle { .. })
        });
        assert_structure(&original.attributes["items"], &snapshot.attributes["items"]);
    }
    let mut owner = NativeDesiredNormalization::from_core(&mut original).unwrap();
    normalize(&mut owner.view());
    drop(owner); // no implicit commit
    assert!(original.attributes.contains_key("items"));
}

#[test]
fn foreign_owner_is_rejected_even_when_slot_numbers_and_resource_ids_match() {
    let first =
        Resource::new("test", "example").with_attribute("items", list(vec![secret("first")]));
    let second =
        Resource::new("test", "example").with_attribute("items", list(vec![secret("second")]));
    let a = NormalizationBatch::new(&[first]).unwrap();
    let b = NormalizationBatch::new(&[second]).unwrap();
    let mut a = DesiredResource::from_wire(a.resources()[0].clone())
        .into_normalization()
        .unwrap();
    let mut b = DesiredResource::from_wire(b.resources()[0].clone())
        .into_normalization()
        .unwrap();
    std::mem::swap(&mut a.attributes, &mut b.attributes);
    assert!(matches!(
        a.finish().unwrap_err().kind,
        Kind::ForeignHandle { .. }
    ));
}

#[test]
fn known_defaults_reject_occupied_known_and_deferred_keys() {
    let original = Resource::new("test", "example").with_attribute(
        "settings",
        map([("known", string("value")), ("pending", reference())]),
    );
    both(original, |view| {
        let Some(DesiredValueMut::Map(map)) = view.attribute_mut("settings") else {
            panic!()
        };
        for name in ["known", "pending"] {
            assert!(matches!(
                map.insert_absent(name, scalar("default")),
                Err(DesiredEditError::Occupied { .. })
            ));
        }
        map.insert_absent("absent", scalar("default")).unwrap();
    });
}

#[test]
fn move_within_preserves_enum_leaves_in_maps_on_both_routes() {
    use carina_core::resource::EnumValueResolver;
    use carina_core::schema::{CompiledAttributeType, TypeIdentity};
    let ty = CompiledAttributeType::enum_(
        TypeIdentity::new(Some("test"), ["Rule"], "Status"),
        Some(vec!["Enabled".into(), "Disabled".into()]),
        Vec::new(),
        None,
        None,
    );
    let rules = ["Enabled", "Disabled"]
        .into_iter()
        .map(|status| {
            map([(
                "status",
                Value::Concrete(ConcreteValue::CanonicalEnum(
                    EnumValueResolver::new(&ty)
                        .resolve_state_text(status)
                        .unwrap(),
                )),
            )])
        })
        .collect();
    let original = Resource::new("test", "list-maps").with_attribute("rules", list(rules));
    let (native, host) = both(original, |view| {
        let Some(DesiredValueMut::List(rules)) = view.attribute_mut("rules") else {
            panic!()
        };
        rules.move_within(1, 0).unwrap();
    });
    for resource in [native, host] {
        let Value::Concrete(ConcreteValue::List(rules)) = &resource.attributes["rules"] else {
            panic!()
        };
        for (rule, expected) in rules.iter().zip(["Disabled", "Enabled"]) {
            let Value::Concrete(ConcreteValue::Map(rule)) = rule else {
                panic!()
            };
            let Value::Concrete(ConcreteValue::CanonicalEnum(status)) = &rule["status"] else {
                panic!("moved status lost its enum kind")
            };
            assert_eq!(status.api_value(), expected);
        }
    }
}

#[test]
fn metadata_and_typed_values_survive_no_op_normalization() {
    let mut resource = Resource::new("test", "example")
        .with_attribute(
            "strings",
            Value::Concrete(ConcreteValue::StringList(vec!["one".into()])),
        )
        .with_attribute(
            "duration",
            Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(30))),
        )
        .with_attribute(
            "map",
            map([
                (
                    "strings",
                    Value::Concrete(ConcreteValue::StringList(vec!["two".into()])),
                ),
                ("secret", secret("private")),
            ]),
        );
    resource.directives.force_delete = true;
    resource.prefixes.insert("strings".into(), "prefix".into());
    let (native, host) = both(resource, |_| {});
    assert!(native.directives.force_delete && host.directives.force_delete);
    assert_eq!(native.prefixes, host.prefixes);
    for resource in [&native, &host] {
        assert!(matches!(
            resource.attributes["duration"],
            Value::Concrete(ConcreteValue::Duration(_))
        ));
        assert!(matches!(
            resource.attributes["strings"],
            Value::Concrete(ConcreteValue::StringList(_))
        ));
        let Value::Concrete(ConcreteValue::Map(map)) = &resource.attributes["map"] else {
            panic!()
        };
        assert!(matches!(
            map["strings"],
            Value::Concrete(ConcreteValue::StringList(_))
        ));
    }
}

#[test]
fn string_list_kinds_survive_appends_on_both_routes() {
    let original = Resource::new("test", "string-lists")
        .with_attribute(
            "top",
            Value::Concrete(ConcreteValue::StringList(vec!["one".into()])),
        )
        .with_attribute(
            "settings",
            map([
                (
                    "nested",
                    Value::Concrete(ConcreteValue::StringList(vec!["two".into()])),
                ),
                ("pending", secret("private")),
            ]),
        );
    let (native, host) = both(original, |view| {
        let Some(DesiredValueMut::List(top)) = view.attribute_mut("top") else {
            panic!()
        };
        top.push(scalar("tail"));
        let Some(DesiredValueMut::Map(settings)) = view.attribute_mut("settings") else {
            panic!()
        };
        let Some(DesiredValueMut::List(nested)) = settings.get_mut("nested") else {
            panic!()
        };
        nested.push(scalar("tail"));
    });
    for resource in [native, host] {
        let Value::Concrete(ConcreteValue::StringList(top)) = &resource.attributes["top"] else {
            panic!()
        };
        assert_eq!(
            top.iter()
                .map(|value| value.expose_plaintext())
                .collect::<Vec<_>>(),
            ["one", "tail"]
        );
        let Value::Concrete(ConcreteValue::Map(settings)) = &resource.attributes["settings"] else {
            panic!()
        };
        let Value::Concrete(ConcreteValue::StringList(nested)) = &settings["nested"] else {
            panic!()
        };
        assert_eq!(
            nested
                .iter()
                .map(|value| value.expose_plaintext())
                .collect::<Vec<_>>(),
            ["two", "tail"]
        );
    }
}

#[test]
fn conversion_errors_preserve_context_and_typed_source_chain() {
    let original = Resource::new("test", "example")
        .with_attribute("settings", map([("known", string("value"))]));
    let batch = NormalizationBatch::new(std::slice::from_ref(&original)).unwrap();
    let mut owner = DesiredResource::from_wire(batch.resources()[0].clone())
        .into_normalization()
        .unwrap();
    let mut view = owner.view();
    let Some(DesiredValueMut::Map(map)) = view.attribute_mut("settings") else {
        panic!()
    };
    map.insert_absent(
        "invalid",
        DesiredValue::Known(DesiredScalar::Float(f64::NAN)),
    )
    .unwrap();
    let error = owner.finish().unwrap_err();
    assert_eq!(error.attribute, "settings");
    assert_eq!(error.path, vec![Segment::Key("invalid".into())]);
    assert!(matches!(
        error
            .source()
            .unwrap()
            .downcast_ref::<ProtocolConvertError>(),
        Some(ProtocolConvertError::NonFiniteFloat)
    ));
}

#[test]
fn native_decode_errors_preserve_nested_context_and_leave_input_unchanged() {
    let mut original = Resource::new("test", "bad-input").with_attribute(
        "settings",
        map([(
            "nested",
            list(vec![Value::Concrete(ConcreteValue::Float(f64::INFINITY))]),
        )]),
    );
    let snapshot = original.clone();
    let error = match NativeDesiredNormalization::from_core(&mut original) {
        Err(error) => error,
        Ok(_) => panic!("non-finite known input must fail conversion"),
    };
    let error = native_cause(&error);
    assert_eq!(error.resource_id.identity, "bad-input");
    assert_eq!(error.attribute, "settings");
    assert_eq!(
        error.path,
        vec![Segment::Key("nested".into()), Segment::Index(0)]
    );
    assert!(error.source().unwrap().is::<ProtocolConvertError>());
    assert_structure(
        &original.attributes["settings"],
        &snapshot.attributes["settings"],
    );
}

#[test]
fn native_question_mark_errors_keep_the_original_provider_instance() {
    use carina_core::provider::ProviderResult;
    fn normalize(resource: &mut Resource, stage: u8) -> ProviderResult<()> {
        if stage == 3 {
            NativeDesiredNormalization::from_core(resource)?.inputs();
            return Ok(());
        }
        let mut owner = NativeDesiredNormalization::from_core(resource)?;
        if stage == 1 {
            let mut view = owner.view();
            let Some(DesiredValueMut::Known(DesiredScalar::Float(value))) =
                view.attribute_mut("value")
            else {
                panic!()
            };
            *value = f64::NAN;
        } else if stage == 2 {
            drop(owner.view().remove("pending"));
        }
        owner.finish()?;
        Ok(())
    }
    for stage in 0..4 {
        let mut resource =
            Resource::with_provider("mock", "example", "native-error", Some("west".into()))
                .with_attribute(
                    "value",
                    Value::Concrete(ConcreteValue::Float(if stage == 0 || stage == 3 {
                        f64::NAN
                    } else {
                        1.0
                    })),
                )
                .with_attribute("pending", secret("private"));
        let id = resource.id.clone();
        let error = normalize(&mut resource, stage).unwrap_err();
        assert_eq!(
            error.detail().resource_id.as_deref(),
            Some(&id),
            "stage={stage}"
        );
        assert!(
            error
                .detail()
                .cause()
                .unwrap()
                .is::<DesiredNormalizationError>()
        );
        assert!(resource.attributes.contains_key("pending"));
    }
}

#[test]
fn native_queries_preserve_pending_identity_and_provider_instance() {
    let id = ResourceId::pending_with_provider("aws", "ec2.NatGateway", Some("west".into()));
    let mut resource: Resource = Resource::from_id(id.clone());
    let mut owner = NativeDesiredNormalization::from_core(&mut resource).unwrap();
    assert_eq!(owner.inputs().id(), &id);
    assert_eq!(owner.view().id(), &id);
    owner.finish().unwrap();
    assert_eq!(resource.id, id);
}

#[tokio::test]
async fn provider_ready_inputs_are_infallible_and_preserve_checked_scalar_and_secret_views() {
    with_type_checks(async |type_checks| {
        use carina_core::executor::{
            ModuleConstraintGate, ProviderPreparationContext, prepare_create_request,
            prepare_provider_ready_data_source,
        };
        use carina_core::provider::NoopNormalizer;
        use carina_core::schema::{
            PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, SchemaRegistry,
        };
        let mut schemas = SchemaRegistry::new();
        let schema = PatternResourceSchema::new("sqs.Queue")
            .attribute(
                PatternAttributeSchema::optional(
                    "tags",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let witness = carina_core::schema::ValidatedProviderSchemaWitness::new(
            vec![schema],
            carina_core::schema::ValidatedConfigSchema::validate(Default::default()).unwrap(),
            Default::default(),
        )
        .unwrap();
        schemas.insert_provider_witness("test", &witness).unwrap();
        let config = carina_core::parser::ExpandedProviderConfig {
            name: "test".into(),
            attributes: Default::default(),
            default_tags: [("unread_secret".into(), secret("private-late-tag"))]
                .into_iter()
                .collect(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        };
        let configs = [config];
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let gate = ModuleConstraintGate::from_live_compositions(&[]);
        let context = ProviderPreparationContext::new(
            &bindings,
            &gate,
            &configs,
            &NoopNormalizer,
            &[],
            &schemas,
            type_checks,
        );
        let resource = Resource::with_provider("test", "sqs.Queue", "queue", None).with_attribute(
            "unread",
            list(vec![Value::Concrete(ConcreteValue::Duration(
                std::time::Duration::from_secs(i64::MAX as u64),
            ))]),
        );
        let request = prepare_create_request(resource, &context).await.unwrap();
        let inputs = DesiredInputs::from_provider_ready(request.resource());
        assert_eq!(inputs.id(), &request.resource().id);
        let Some(DesiredValue::List(values)) = inputs.attribute("unread") else {
            panic!()
        };
        assert!(matches!(
            values.iter().next(),
            Some(DesiredValue::Known(DesiredScalar::Int(i64::MAX)))
        ));
        let Some(DesiredValue::Map(tags)) = inputs.attribute("tags") else {
            panic!()
        };
        assert!(matches!(
            tags.get("unread_secret"),
            Some(DesiredValue::Deferred(_))
        ));

        let mut data =
            carina_core::resource::DataSource::with_provider("test", "lookup", "lookup", None);
        data.attributes.insert(
            "unread".into(),
            Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
                i64::MAX as u64,
            ))),
        );
        let ready = prepare_provider_ready_data_source(data, &context).unwrap();
        let inputs = DesiredInputs::from_provider_ready(&ready);
        assert!(matches!(
            inputs.attribute("unread"),
            Some(DesiredValue::Known(DesiredScalar::Int(i64::MAX)))
        ));
    })
    .await
}

#[tokio::test]
async fn provider_ready_inputs_preserve_env_string_diagnostic_sensitivity() {
    with_type_checks(async |type_checks| {
        use carina_core::executor::{
            ModuleConstraintGate, ProviderPreparationContext, prepare_create_request,
        };
        let sensitive =
            carina_core::builtins::evaluate_builtin_to_value("env", &[string("PATH")]).unwrap();
        let Value::Concrete(ConcreteValue::String(text)) = &sensitive else {
            panic!("env returns a string")
        };
        assert!(text.diagnostic_literal().is_none());
        let mut resource = Resource::with_provider("test", "test.Resource", "sensitive", None)
            .with_attribute("token", sensitive.clone())
            .with_attribute("nested", map([("token", sensitive.clone())]))
            .with_attribute("list", list(vec![sensitive.clone()]))
            .with_attribute(
                "string_list",
                Value::Concrete(ConcreteValue::StringList(vec![text.clone()])),
            );
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = carina_core::schema::SchemaRegistry::new();
        let context = ProviderPreparationContext::new(
            &bindings,
            &gate,
            &[],
            &carina_core::provider::NoopNormalizer,
            &[],
            &schemas,
            type_checks,
        );
        let request = prepare_create_request(resource.clone(), &context)
            .await
            .unwrap();
        let ready = DesiredInputs::from_provider_ready(request.resource());
        let owner = NativeDesiredNormalization::from_core(&mut resource).unwrap();
        for inputs in [ready, owner.inputs()] {
            let Some(DesiredValue::Known(DesiredScalar::String(text))) = inputs.attribute("token")
            else {
                panic!("known string")
            };
            assert!(text.diagnostic_literal().is_none());
            let Some(DesiredValue::Map(nested)) = inputs.attribute("nested") else {
                panic!()
            };
            let Some(DesiredValue::Known(DesiredScalar::String(text))) = nested.get("token") else {
                panic!()
            };
            assert!(text.diagnostic_literal().is_none());
            for name in ["list", "string_list"] {
                let Some(DesiredValue::List(values)) = inputs.attribute(name) else {
                    panic!()
                };
                let Some(DesiredValue::Known(DesiredScalar::String(text))) = values.iter().next()
                else {
                    panic!()
                };
                assert!(text.diagnostic_literal().is_none(), "{name}");
            }
        }
    })
    .await
}

#[tokio::test]
async fn native_and_wasm_views_share_sorted_order_for_unsorted_authored_maps() {
    use carina_core::executor::{
        ModuleConstraintGate, ProviderPreparationContext, prepare_create_request,
    };
    let mut resource = Resource::with_provider("test", "test.Resource", "ordered", None);
    resource.attributes = IndexMap::from([
        (
            "z".into(),
            map([("z", string("last")), ("a", string("first"))]),
        ),
        ("a".into(), string("first")),
    ]);
    assert_eq!(
        resource
            .attributes
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    let Value::Concrete(ConcreteValue::Map(authored_nested)) = &resource.attributes["z"] else {
        panic!()
    };
    assert_eq!(
        authored_nested
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["z", "a"]
    );

    let batch = NormalizationBatch::new(std::slice::from_ref(&resource)).unwrap();
    let mut wire = batch.resources()[0].clone();
    roundtrip(&mut wire);
    let mut guest = DesiredResource::from_wire(wire)
        .into_normalization()
        .unwrap();
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = carina_core::schema::SchemaRegistry::new();
    let request = with_type_checks(async |type_checks| {
        let context = ProviderPreparationContext::new(
            &bindings,
            &gate,
            &[],
            &carina_core::provider::NoopNormalizer,
            &[],
            &schemas,
            type_checks,
        );
        prepare_create_request(resource.clone(), &context)
            .await
            .unwrap()
    })
    .await;
    let ready = DesiredInputs::from_provider_ready(request.resource());
    let native = NativeDesiredNormalization::from_core(&mut resource).unwrap();
    let native_inputs = native.inputs();
    let guest_view = guest.view();
    let guest_inputs = guest_view.inputs();
    let map_names = |inputs: &DesiredInputs<'_>| {
        let tree = match &inputs.tree {
            InputsTree::Borrowed(tree) => tree,
            InputsTree::Owned(tree) => tree,
        };
        let Some(DesiredValue::Map(nested)) = tree.get("z") else {
            panic!()
        };
        (
            tree.entries.keys().cloned().collect::<Vec<_>>(),
            nested.entries.keys().cloned().collect::<Vec<_>>(),
        )
    };
    let expected = (
        vec!["a".to_owned(), "z".to_owned()],
        vec!["a".to_owned(), "z".to_owned()],
    );
    for inputs in [&ready, &native_inputs, &guest_inputs] {
        assert_eq!(map_names(inputs), expected);
    }
    assert_eq!(map_names(&ready), map_names(&guest_inputs));
    assert_eq!(map_names(&native_inputs), map_names(&guest_inputs));
}
