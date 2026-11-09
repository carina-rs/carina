use super::*;
use crate::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, LoadedProviderSnapshot,
    ProviderInitialization, ProviderResult,
};
use crate::schema::{
    AttributeSchema, AttributeType as DraftType, RawShape, ResourceSchema, SchemaKind,
    TypeInSchema, ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

struct Factory {
    owner: &'static str,
    declarations: Vec<DraftType>,
    config: BTreeMap<String, DraftType>,
}

impl ProviderFactory for Factory {
    fn name(&self) -> &str {
        self.owner
    }
    fn display_name(&self) -> &str {
        self.owner
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::validate(self.config.clone()).unwrap()
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _session: &crate::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &crate::provider::ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { panic!("annotation fixtures only validate") })
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let schema = self.declarations.iter().enumerate().fold(
            ResourceSchema::new("Values"),
            |schema, (index, declaration)| {
                schema.attribute(
                    AttributeSchema::optional(format!("value{index}"), declaration.clone()).build(),
                )
            },
        );
        vec![schema.validate().unwrap()]
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }
}

fn text(value: impl Into<String>) -> Value {
    Value::Concrete(crate::resource::ConcreteValue::String(value.into().into()))
}
fn integer(value: i64) -> Value {
    Value::Concrete(crate::resource::ConcreteValue::Int(value))
}
fn list(values: Vec<Value>) -> Value {
    Value::Concrete(crate::resource::ConcreteValue::List(values))
}
fn map(values: indexmap::IndexMap<String, Value>) -> Value {
    Value::Concrete(crate::resource::ConcreteValue::Map(values))
}

fn identity() -> TypeIdentity {
    TypeIdentity::from_schema_type("aws", "iam.Role", "Name")
}

fn string(min: u64, max: u64, pattern: Option<crate::schema::PatternSpec>) -> DraftType {
    DraftType::refined_string(
        Some(identity()),
        pattern,
        Some((Some(min), Some(max))),
        None,
    )
}

fn member(min: u64, max: u64, pattern: Option<crate::schema::PatternSpec>) -> DraftType {
    DraftType::string_member(
        crate::schema::StringIdentityLayer::new(Some(identity()), None, None, None),
        crate::schema::StringMemberConstraints::new(pattern, Some((Some(min), Some(max)))),
    )
}

#[test]
fn read_value_evidence_cannot_retain_string_or_int_member_constraints() {
    for declarations in [
        vec![member(1, 8, None), member(16, 32, None)],
        vec![
            DraftType::int_member(
                crate::schema::IntIdentityLayer::new(Some(identity()), None),
                crate::schema::IntMemberConstraints::new(Some((Some(1), Some(8)))),
            ),
            DraftType::int_member(
                crate::schema::IntIdentityLayer::new(Some(identity()), None),
                crate::schema::IntMemberConstraints::new(Some((Some(16), Some(32)))),
            ),
        ],
    ] {
        let loaded = snapshot(vec![("p", declarations)]);
        let left = inference::attribute_type_to_type_expr(declaration(&loaded, "p", 0), "p");
        let right = inference::attribute_type_to_type_expr(declaration(&loaded, "p", 1), "p");
        assert_eq!(
            left, right,
            "a destination member layer is not value type evidence"
        );
        let wire = serde_json::to_string(&left).unwrap();
        assert!(
            !wire.contains("member_layer") && !wire.contains("member_range"),
            "{wire}"
        );
        let restored: TypeExpr = serde_json::from_str(&wire).unwrap();
        assert_eq!(left, restored);
        let value = match &left {
            TypeExpr::SchemaType { owned }
                if owned.scalar_kind() == crate::schema::RefinedScalarKind::Int =>
            {
                integer(100)
            }
            _ => text("value outside either slot's length bounds"),
        };
        typecheck::check_type_expr_value(
            &restored,
            &value,
            loaded.provider_context(),
            &crate::parser::retained::value_dependent_judgement_for_test(),
        )
        .require_provider_schemas()
        .expect("reads retain identity constraints and semantic checks only");
    }
}

fn local(declaration: DraftType) -> DraftType {
    let RawShape::String {
        identity_layer,
        member_layer,
    } = declaration.raw_shape()
    else {
        return declaration;
    };
    let crate::schema::StringIdentityParts {
        identity,
        pattern,
        length,
        validate,
        to_dsl,
    } = identity_layer;
    assert!(member_layer.pattern.is_none() && member_layer.length.is_none());
    let base = match validate {
        Some(validate) => crate::schema::StringIdentityLayer::with_validator(
            identity.unwrap().clone(),
            None,
            None,
            validate.clone(),
            to_dsl.cloned(),
        ),
        None => {
            crate::schema::StringIdentityLayer::new(identity.cloned(), None, None, to_dsl.cloned())
        }
    };
    DraftType::string_member(
        base,
        crate::schema::StringMemberConstraints::new(pattern.cloned(), length),
    )
}

fn snapshot(owners: Vec<(&'static str, Vec<DraftType>)>) -> LoadedProviderSnapshot {
    snapshot_with_config(owners, BTreeMap::new())
}

fn snapshot_with_config(
    owners: Vec<(&'static str, Vec<DraftType>)>,
    config: BTreeMap<String, DraftType>,
) -> LoadedProviderSnapshot {
    let factories = owners
        .into_iter()
        .map(|(owner, declarations)| {
            Box::new(Factory {
                owner,
                declarations,
                config: config.clone(),
            }) as Box<dyn ProviderFactory>
        })
        .collect();
    crate::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        crate::EnvironmentSnapshot::empty(),
    )
}

fn annotation(snapshot: &LoadedProviderSnapshot) -> TypeExpr {
    let context = snapshot.provider_context().as_provider_context();
    let ty = crate::parser::parse_type_expr_str(&identity().to_string(), context).unwrap();
    resolve_type_expr(&ty, context).unwrap()
}

fn declaration<'a>(
    snapshot: &'a LoadedProviderSnapshot,
    owner: &str,
    index: usize,
) -> &'a AttributeType {
    snapshot
        .schema_registry()
        .get(owner, "Values", SchemaKind::Resource)
        .unwrap()
        .schema()
        .parts()
        .attributes[&format!("value{index}")]
        .attr_type()
}

fn assign(
    source: &AttributeType,
    sink: &AttributeType,
    snapshot: &LoadedProviderSnapshot,
    owner: &str,
) -> bool {
    let owner = crate::parser::SchemaTypeOwner::Provider(owner.into());
    source
        .in_schema(crate::schema::empty_defs_for_schema_walks())
        .is_assignable_to(
            sink.in_schema(crate::schema::empty_defs_for_schema_walks())
                .with_authority(
                    &snapshot
                        .provider_context()
                        .as_provider_context()
                        .custom_types,
                    crate::schema::SinkAuthority::Owner(&owner),
                ),
        )
}

#[test]
fn lifting_preserves_all_string_obligations_in_nested_annotations() {
    let snapshot = snapshot(vec![(
        "awscc",
        vec![string(
            1,
            64,
            Some(crate::schema::PatternSpec::new(
                "^[A-Z]+$",
                crate::schema::PatternMatch::Full,
            )),
        )],
    )]);
    let ty = annotation(&snapshot);
    let lifted = lift_type_expr(&ty).unwrap();
    assert!(matches!(
        lifted.raw_shape(),
        RawShape::String {
            identity_layer: crate::schema::StringIdentityParts {
                length: Some((Some(1), Some(64))),
                ..
            },
            ..
        }
    ));
    assert!(lifted.validate(&text("VALID")).is_ok());
    assert!(lifted.validate(&text("invalid")).is_err());
    assert!(lifted.validate(&text("A".repeat(65))).is_err());
    let nested = lift_type_expr(&TypeExpr::List(Box::new(TypeExpr::Map(Box::new(ty))))).unwrap();
    assert!(
        nested
            .validate(&list(vec![map(indexmap::IndexMap::from([(
                "name".into(),
                text("invalid")
            )]))]))
            .is_err()
    );
}

#[test]
fn identity_annotations_ignore_member_order_and_keep_every_owner() {
    use crate::schema::{
        PatternMatch, PatternReason, PatternSpec, StringIdentityLayer, StringMemberConstraints,
    };
    let base = StringIdentityLayer::new(
        Some(identity()),
        Some(PatternSpec::new("[A-Z]+", PatternMatch::Full)),
        Some((Some(8), Some(64))),
        None,
    );
    let site = |regex: &str, min, max| {
        DraftType::string_member(
            base.clone(),
            StringMemberConstraints::new(
                Some(PatternSpec::new(
                    regex,
                    PatternMatch::Substring {
                        reason: PatternReason::new("A member-specific prefix or suffix").unwrap(),
                    },
                )),
                Some((Some(min), Some(max))),
            ),
        )
    };
    let first = vec![site("^A", 1, 64), site("Z$", 8, 128)];
    let second = vec![site("[A-Z]+", 4, 80)];
    let a = snapshot(vec![("p", first.clone()), ("q", second.clone())]);
    let b = snapshot(vec![
        ("q", second),
        ("p", first.into_iter().rev().collect()),
    ]);
    assert_eq!(annotation(&a), annotation(&b));
    let first = inference::attribute_type_to_type_expr(declaration(&a, "p", 0), "p");
    let second = inference::attribute_type_to_type_expr(declaration(&a, "p", 1), "p");
    assert_eq!(
        first, second,
        "member differences disappear at the read seam"
    );
    let lifted = lift_type_expr(&annotation(&a)).unwrap();
    for value in ["AAAAAAAZ", "BAAAAAAZ", "AAAAAAAA"] {
        assert!(lifted.validate(&text(value)).is_ok());
    }
    for value in ["AaaaaaaZ", "AZ"] {
        assert!(lifted.validate(&text(value)).is_err());
    }
    for (owner, index) in [("p", 0), ("p", 1), ("q", 0)] {
        assert!(assign(&lifted, declaration(&a, owner, index), &a, owner));
    }
    let restored: TypeExpr =
        serde_json::from_value(serde_json::to_value(annotation(&a)).unwrap()).unwrap();
    assert_eq!(restored, annotation(&a));
    assert!(
        lift_type_expr(&restored)
            .unwrap()
            .validate(&text("BAAAAAAZ"))
            .is_ok()
    );
}

#[test]
fn numeric_annotation_intersections_preserve_effective_ranges() {
    for (declarations, expected) in [
        (
            vec![
                DraftType::int_member(
                    crate::schema::IntIdentityLayer::new(
                        Some(identity()),
                        Some((Some(8), Some(64))),
                    ),
                    crate::schema::IntMemberConstraints::new(Some((Some(-2), Some(64)))),
                ),
                DraftType::int_member(
                    crate::schema::IntIdentityLayer::new(
                        Some(identity()),
                        Some((Some(8), Some(64))),
                    ),
                    crate::schema::IntMemberConstraints::new(Some((Some(8), Some(128)))),
                ),
            ],
            "range 8..=64",
        ),
        (
            vec![
                DraftType::refined_float(
                    Some(identity()),
                    Some((Some(crate::finite(-0.5)), Some(crate::finite(64.5)))),
                ),
                DraftType::refined_float(
                    Some(identity()),
                    Some((Some(crate::finite(8.5)), Some(crate::finite(128.5)))),
                ),
            ],
            "range 8.5..=64.5",
        ),
    ] {
        let a = snapshot(vec![("p", declarations.clone())]);
        let b = snapshot(vec![("p", declarations.into_iter().rev().collect())]);
        assert_eq!(annotation(&a), annotation(&b));
        let lifted = lift_type_expr(&annotation(&a)).unwrap();
        assert!(assign(&lifted, declaration(&a, "p", 0), &a, "p"));
        assert!(assign(&lifted, declaration(&a, "p", 1), &a, "p"));
        assert!(lifted.validate(&integer(16)).is_ok());
        assert!(lifted.validate(&integer(7)).is_err());
        assert!(lifted.validate(&integer(65)).is_err());
        let (expected_name, actual) = TypeInSchema::schemaless(&lifted)
            .mismatch_type_names(TypeInSchema::schemaless(declaration(&a, "p", 0)));
        assert!(actual.contains(expected), "{actual}");
        if matches!(lifted.raw_shape(), RawShape::Int { .. }) {
            assert!(
                expected_name.contains("member range -2..=64"),
                "{expected_name}"
            );
        }
    }
}

#[test]
fn nominal_pattern_policy_is_preserved_and_diagnostics_show_each_pattern() {
    let a = snapshot(vec![(
        "p",
        vec![string(
            1,
            64,
            Some(crate::schema::PatternSpec::new(
                "^A",
                crate::schema::PatternMatch::Substring {
                    reason: crate::schema::PatternReason::new(
                        "Owner contributes a prefix or suffix constraint to the conjunction",
                    )
                    .unwrap(),
                },
            )),
        )],
    )]);
    let b = snapshot(vec![(
        "p",
        vec![string(
            1,
            64,
            Some(crate::schema::PatternSpec::new(
                "Z$",
                crate::schema::PatternMatch::Substring {
                    reason: crate::schema::PatternReason::new(
                        "Owner contributes a prefix or suffix constraint to the conjunction",
                    )
                    .unwrap(),
                },
            )),
        )],
    )]);
    let source = lift_type_expr(&annotation(&a)).unwrap();
    let sink = declaration(&b, "p", 0);
    assert!(
        assign(&source, sink, &b, "p"),
        "nominal assignment must keep its existing pattern policy"
    );
    let (expected, actual) =
        TypeInSchema::schemaless(&source).mismatch_type_names(TypeInSchema::schemaless(sink));
    assert!(expected.contains("pattern \"Z$\""), "{expected}");
    assert!(actual.contains("pattern \"^A\""), "{actual}");
    let source = AttributeType::refined_string(
        None,
        Some(
            crate::schema::CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                "^A",
                crate::schema::PatternMatch::Substring {
                    reason: crate::schema::PatternReason::new(
                        "Owner contributes a prefix constraint to the conjunction",
                    )
                    .unwrap(),
                },
            ))
            .unwrap(),
        ),
        None,
        None,
    );
    let sink = AttributeType::refined_string(
        None,
        Some(
            crate::schema::CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                "Z$",
                crate::schema::PatternMatch::Substring {
                    reason: crate::schema::PatternReason::new(
                        "Owner contributes a suffix constraint to the conjunction",
                    )
                    .unwrap(),
                },
            ))
            .unwrap(),
        ),
        None,
        None,
    );
    assert!(!assign(&source, &sink, &b, "p"));
    let (expected, actual) =
        TypeInSchema::schemaless(&source).mismatch_type_names(TypeInSchema::schemaless(&sink));
    assert!(
        expected.contains("length unbounded, pattern \"Z$\""),
        "{expected}"
    );
    assert!(
        actual.contains("length unbounded, pattern \"^A\""),
        "{actual}"
    );
}

#[test]
fn empty_intersections_are_explicit_and_reject_values_and_assignments() {
    for (first, second) in [
        (string(1, 4, None), string(8, 12, None)),
        (
            DraftType::refined_int(Some(identity()), Some((Some(1), Some(4)))),
            DraftType::refined_int(Some(identity()), Some((Some(8), Some(12)))),
        ),
        (
            DraftType::refined_float(
                Some(identity()),
                Some((Some(crate::finite(1.0)), Some(crate::finite(4.0)))),
            ),
            DraftType::refined_float(
                Some(identity()),
                Some((Some(crate::finite(8.0)), Some(crate::finite(12.0)))),
            ),
        ),
    ] {
        let snapshot = snapshot(vec![("p", vec![first]), ("q", vec![second])]);
        let ty = annotation(&snapshot);
        let lifted = lift_type_expr(&ty).unwrap();
        assert!(!assign(
            &lifted,
            declaration(&snapshot, "p", 0),
            &snapshot,
            "p"
        ));
        assert!(!assign(
            declaration(&snapshot, "p", 0),
            &lifted,
            &snapshot,
            "p"
        ));
        assert!(!assign(&lifted, &lifted, &snapshot, "p"));
        let value = match lifted.raw_shape() {
            RawShape::String { .. } => text("ABC"),
            _ => integer(3),
        };
        assert!(lifted.validate(&value).is_err());
        let wire = serde_json::to_value(&ty).unwrap();
        assert_eq!(
            wire["SchemaType"]["owned"]["constraints"]["empty_intersection"],
            true
        );
        let restored: TypeExpr = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, ty);
        assert!(
            lift_type_expr(&restored)
                .unwrap()
                .has_empty_scalar_intersection()
        );
    }
}

#[test]
fn read_projections_keep_identity_constraints_without_slot_members() {
    let snapshot = snapshot(vec![("p", vec![member(1, 128, None), member(8, 64, None)])]);
    let source = declaration(&snapshot, "p", 0);
    let exact = inference::attribute_type_to_type_expr(source, "p");
    let lifted = lift_type_expr(&exact).unwrap();
    assert!(matches!(
        lifted.raw_shape(),
        RawShape::String {
            member_layer: crate::schema::StringMemberParts { length: None, .. },
            ..
        }
    ));
    assert_ne!(exact, annotation(&snapshot));
    assert!(assign(
        &lifted,
        declaration(&snapshot, "p", 1),
        &snapshot,
        "p"
    ));
    typecheck::check_type_expr_value(
        &exact,
        &text("A".repeat(80)),
        snapshot.provider_context(),
        &crate::parser::retained::value_dependent_judgement_for_test(),
    )
    .require_provider_schemas()
    .expect("an exact source value must not inherit its narrower sibling's bound");
    let (expected, actual) = TypeInSchema::schemaless(&lifted)
        .mismatch_type_names(TypeInSchema::schemaless(declaration(&snapshot, "p", 1)));
    assert_eq!(
        expected,
        "aws.iam.Role.Name (length unbounded, member length 8..=64)"
    );
    assert_eq!(actual, "aws.iam.Role.Name (length unbounded)");
    let wire = serde_json::to_value(&exact).unwrap();
    assert_eq!(wire["SchemaType"]["owned"]["origin"], "Projection");
    let restored: TypeExpr = serde_json::from_value(wire).unwrap();
    assert_eq!(restored, exact);
    typecheck::check_type_expr_value(
        &restored,
        &text("A".repeat(80)),
        snapshot.provider_context(),
        &crate::parser::retained::value_dependent_judgement_for_test(),
    )
    .require_provider_schemas()
    .expect("serde must retain the exact projection's validation scope");
}

#[test]
fn exact_projection_value_validation_keeps_its_callback_without_sibling_callbacks() {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let wide_calls = Arc::clone(&calls);
    let narrow_calls = Arc::clone(&calls);
    let wide = DraftType::refined_string_with_validator(
        identity(),
        None,
        Some((Some(1), Some(128))),
        crate::schema::legacy_validator(move |_| {
            wide_calls.lock().unwrap().push("wide");
            Ok(())
        }),
        None,
    );
    let narrow = DraftType::refined_string_with_validator(
        identity(),
        None,
        Some((Some(8), Some(64))),
        crate::schema::legacy_validator(move |_| {
            narrow_calls.lock().unwrap().push("narrow");
            Err("sibling callback rejected".into())
        }),
        None,
    );
    let snapshot = snapshot(vec![("p", vec![local(wide), local(narrow)])]);
    let exact = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 0), "p");
    let value = text("A".repeat(16));
    typecheck::check_type_expr_value(
        &exact,
        &value,
        snapshot.provider_context(),
        &crate::parser::retained::value_dependent_judgement_for_test(),
    )
    .require_provider_schemas()
    .expect("only the exact declaration's callback may validate this source");
    assert_eq!(*calls.lock().unwrap(), ["wide"]);
    let error = typecheck::check_type_expr_value(
        &annotation(&snapshot),
        &value,
        snapshot.provider_context(),
        &crate::parser::retained::value_dependent_judgement_for_test(),
    )
    .require_provider_schemas()
    .unwrap_err();
    assert!(
        error.to_string().contains("sibling callback rejected"),
        "{error}"
    );
    assert_eq!(*calls.lock().unwrap(), ["wide", "wide", "narrow"]);
}

#[test]
fn config_only_declarations_join_resource_declarations_and_truly_unbounded_types_lift() {
    let snapshot = snapshot_with_config(
        vec![("p", vec![string(8, 64, None)])],
        BTreeMap::from([("config_name".into(), string(8, 64, None))]),
    );
    let lifted = lift_type_expr(&annotation(&snapshot)).unwrap();
    assert!(matches!(
        lifted.raw_shape(),
        RawShape::String {
            identity_layer: crate::schema::StringIdentityParts {
                length: Some((Some(8), Some(64))),
                ..
            },
            ..
        }
    ));
    let config_only = snapshot_with_config(
        vec![("p", vec![])],
        BTreeMap::from([("config_name".into(), string(2, 16, None))]),
    );
    assert!(matches!(
        lift_type_expr(&annotation(&config_only))
            .unwrap()
            .raw_shape(),
        RawShape::String {
            identity_layer: crate::schema::StringIdentityParts {
                length: Some((Some(2), Some(16))),
                ..
            },
            ..
        }
    ));
    let unbounded = self::snapshot(vec![(
        "p",
        vec![DraftType::refined_string(
            Some(identity()),
            None,
            None,
            None,
        )],
    )]);
    let ty = annotation(&unbounded);
    let wire = serde_json::to_value(&ty).unwrap();
    assert_eq!(
        wire["SchemaType"]["owned"]["constraints"]["scalar"]["String"]["identity_layer"]["length"]
            ["min"],
        "Unbounded"
    );
    let restored: TypeExpr = serde_json::from_value(wire).unwrap();
    assert_eq!(restored, ty);
    assert!(
        lift_type_expr(&restored)
            .unwrap()
            .validate(&text(""))
            .is_ok()
    );
}

#[test]
fn review_projection_serde_rebinds_exact_callbacks_and_enum_semantics() {
    let callback = DraftType::refined_string_with_validator(
        identity(),
        None,
        Some((Some(1), Some(128))),
        crate::schema::legacy_validator(|_| Err("exact callback rejected".into())),
        None,
    );
    let enumeration = DraftType::enum_(identity(), Some(vec!["VALID".into()]), vec![], None, None);
    let dynamic_enum = DraftType::enum_(
        identity(),
        None,
        vec![],
        Some(crate::schema::legacy_validator(|_| {
            Err("exact enum callback rejected".into())
        })),
        None,
    );
    for draft in [local(callback), enumeration, dynamic_enum] {
        let snapshot = snapshot(vec![("p", vec![draft, member(8, 64, None)])]);
        let exact = TypeExpr::SchemaType {
            owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
                "p",
                declaration(&snapshot, "p", 0),
            )
            .unwrap(),
        };
        let restored: TypeExpr =
            serde_json::from_value(serde_json::to_value(&exact).unwrap()).unwrap();
        for ty in [&exact, &restored] {
            assert!(
                typecheck::check_type_expr_value(
                    ty,
                    &text("INVALID"),
                    snapshot.provider_context(),
                    &crate::parser::retained::value_dependent_judgement_for_test(),
                )
                .require_provider_schemas()
                .is_err(),
                "exact validation must survive serde: {ty:?}"
            );
        }
        let missing = self::snapshot(vec![("p", vec![member(1, 128, None)])]);
        assert!(
            typecheck::check_type_expr_value(
                &restored,
                &text("VALID"),
                missing.provider_context(),
                &crate::parser::retained::value_dependent_judgement_for_test(),
            )
            .require_provider_schemas()
            .is_err(),
            "a missing exact declaration cannot become a scalar"
        );
    }
}

#[test]
fn review_projection_serde_keeps_reference_semantics_and_rejects_ambiguous_callbacks() {
    let reference = DraftType::reference(
        identity(),
        Some(crate::schema::PatternSpec::new(
            "^ref-",
            crate::schema::PatternMatch::Full,
        )),
        Some((Some(1), Some(64))),
    );
    let references = snapshot(vec![("p", vec![reference])]);
    let exact = inference::attribute_type_to_type_expr(declaration(&references, "p", 0), "p");
    let wire = serde_json::to_value(&exact).unwrap();
    let restored: TypeExpr = serde_json::from_value(wire.clone()).unwrap();
    let TypeExpr::SchemaType { owned } = &restored else {
        panic!("exact projection")
    };
    assert!(matches!(
        owned
            .resolve_projection(references.provider_context().as_provider_context())
            .unwrap()
            .unwrap()
            .raw_shape(),
        RawShape::Reference { .. }
    ));
    let mut missing_provenance = wire;
    missing_provenance["SchemaType"]["owned"]
        .as_object_mut()
        .unwrap()
        .remove("projection_fingerprint");
    assert!(serde_json::from_value::<TypeExpr>(missing_provenance).is_err());
    let unloaded = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    assert!(matches!(
        owned.resolve_projection(&unloaded),
        Err(crate::parser::CustomTypeValidationError::ProvidersNotLoaded)
    ));

    let callback = |reject| {
        DraftType::refined_string_with_validator(
            identity(),
            None,
            Some((Some(1), Some(64))),
            crate::schema::legacy_validator(move |_| {
                if reject {
                    Err("rejected".into())
                } else {
                    Ok(())
                }
            }),
            None,
        )
    };
    let ambiguous = snapshot(vec![("p", vec![callback(false), callback(true)])]);
    let exact = inference::attribute_type_to_type_expr(declaration(&ambiguous, "p", 0), "p");
    let restored: TypeExpr = serde_json::from_value(serde_json::to_value(exact).unwrap()).unwrap();
    let error = typecheck::check_type_expr_value(
        &restored,
        &text("VALID"),
        ambiguous.provider_context(),
        &crate::parser::retained::value_dependent_judgement_for_test(),
    )
    .require_provider_schemas()
    .unwrap_err();
    assert!(
        error.to_string().contains("ambiguous callback provenance"),
        "{error}"
    );
}

#[test]
fn review_enum_witness_constraints_apply_to_canonical_api_spelling() {
    use crate::resource::{CanonicalEnumValue, ConcreteValue, RawEnumIdentifier};
    let strict = snapshot(vec![(
        "p",
        vec![string(
            4,
            4,
            Some(crate::schema::PatternSpec::new(
                "^[A-Z]+$",
                crate::schema::PatternMatch::Full,
            )),
        )],
    )]);
    let enumeration = DraftType::enum_(
        identity(),
        Some(vec!["GOOD".into()]),
        vec![("GOOD".into(), "long_alias".into())],
        None,
        None,
    );
    let snapshot = snapshot(vec![("p", vec![enumeration])]);
    let ty = annotation(&strict);
    for value in [
        Value::Concrete(ConcreteValue::EnumIdentifier(RawEnumIdentifier::parse(
            format!("{}.long_alias", identity()),
        ))),
        Value::Concrete(ConcreteValue::CanonicalEnum(
            CanonicalEnumValue::new_for_test(identity(), "GOOD"),
        )),
    ] {
        typecheck::check_type_expr_value(
            &ty,
            &value,
            snapshot.provider_context(),
            &crate::parser::retained::value_dependent_judgement_for_test(),
        )
        .require_provider_schemas()
        .unwrap();
    }
}

#[test]
fn review_enum_values_validate_canonical_scalar_against_retained_witness() {
    use crate::resource::{CanonicalEnumValue, ConcreteValue, RawEnumIdentifier};
    let open_enum = |base| DraftType::enum_with_base(identity(), base, None, vec![], None, None);
    for base in [
        string(2, 4, None),
        string(
            1,
            32,
            Some(crate::schema::PatternSpec::new(
                "^[A-Z]+$",
                crate::schema::PatternMatch::Full,
            )),
        ),
    ] {
        let strict = snapshot(vec![("p", vec![open_enum(base)])]);
        let lax = snapshot(vec![("p", vec![open_enum(DraftType::string())])]);
        let ty = annotation(&strict);
        let ty: TypeExpr = serde_json::from_value(serde_json::to_value(ty).unwrap()).unwrap();
        for (spelling, accepted) in [("GOOD", true), ("invalid", false)] {
            for value in [
                Value::Concrete(ConcreteValue::EnumIdentifier(RawEnumIdentifier::parse(
                    format!("{}.{}", identity(), spelling),
                ))),
                Value::Concrete(ConcreteValue::CanonicalEnum(
                    CanonicalEnumValue::new_for_test(identity(), spelling),
                )),
            ] {
                let result = typecheck::check_type_expr_value(
                    &ty,
                    &value,
                    lax.provider_context(),
                    &crate::parser::retained::value_dependent_judgement_for_test(),
                )
                .require_provider_schemas();
                assert_eq!(result.is_ok(), accepted, "{value:?}: {result:?}");
            }
        }
    }
}

fn assert_enum_witness_masks_env_value(base: DraftType, expected: &str) {
    let open_enum = |base| DraftType::enum_with_base(identity(), base, None, vec![], None, None);
    let strict = snapshot(vec![("p", vec![open_enum(base)])]);
    let lax = snapshot(vec![("p", vec![open_enum(DraftType::string())])]);
    let plaintext = "private-token";
    let runtime = ProviderContext::new(crate::EnvironmentSnapshot::new([(
        "ANNOTATION_SECRET".into(),
        plaintext.into(),
    )]));
    let context = lax.provider_context().with_runtime(&runtime);
    let mut parsed = crate::parser::parse(
        "let source = p.Values { value0 = env('ANNOTATION_SECRET') }",
        context.as_provider_context(),
    )
    .unwrap();
    crate::parser::resolve_resource_refs_with_config(&mut parsed, context.as_provider_context())
        .unwrap();
    let env_value = parsed.resources[0].get_attr("value0").unwrap();
    assert!(env_value.contains_diagnostic_sensitive_string());
    let ty: TypeExpr =
        serde_json::from_value(serde_json::to_value(annotation(&strict)).unwrap()).unwrap();
    for value in [env_value, &text(plaintext)] {
        let error = typecheck::check_type_expr_value(
            &ty,
            value,
            &context,
            &crate::parser::retained::value_dependent_judgement_for_test(),
        )
        .require_provider_schemas()
        .expect_err("the retained witness must reject the value");
        let message = error.to_string();
        assert!(message.contains(expected), "{message}");
        if value.contains_diagnostic_sensitive_string() {
            assert!(!message.contains(plaintext), "secret leaked: {message}");
            assert!(!format!("{error:?}").contains(plaintext));
            assert!(message.contains("(secret)"), "{message}");
            assert!(!message.contains("'(secret)'"), "{message}");
        } else {
            assert!(message.contains(plaintext), "{message}");
        }
    }
}

#[test]
fn review_enum_witness_length_error_masks_env_value() {
    assert_enum_witness_masks_env_value(string(2, 4, None), "outside allowed range [2, 4]");
}

#[test]
fn review_enum_witness_pattern_error_masks_env_value() {
    assert_enum_witness_masks_env_value(
        string(
            1,
            32,
            Some(crate::schema::PatternSpec::new(
                "^[A-Z]+$",
                crate::schema::PatternMatch::Full,
            )),
        ),
        "required pattern",
    );
}

#[test]
fn review_unloaded_resolution_waits_for_provider_config_declarations() {
    let snapshot = snapshot_with_config(
        vec![("p", vec![string(8, 64, None)])],
        BTreeMap::from([("config_name".into(), string(8, 64, None))]),
    );
    let unloaded = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let ty = TypeExpr::try_from(identity()).unwrap();
    let preliminary =
        crate::parser::resolve_type_expr_from_schemas(&ty, snapshot.schema_registry(), &unloaded)
            .unwrap();
    assert!(matches!(preliminary, TypeExpr::UnloadedSchemaType(_)));
    let resolved = crate::parser::resolve_type_expr_from_schemas(
        &preliminary,
        snapshot.schema_registry(),
        snapshot.provider_context().as_provider_context(),
    )
    .unwrap();
    assert_eq!(resolved, annotation(&snapshot));
    assert!(
        lift_type_expr(&resolved)
            .unwrap()
            .validate(&text("A".repeat(80)))
            .is_err()
    );
}

#[test]
fn annotation_resolution_rejects_missing_owners_and_defers_partial_catalogs() {
    use crate::parser::{CustomTypeResolver, CustomTypeValidationError, SchemaTypeOwner};
    let snapshot = snapshot(vec![("p", vec![string(1, 64, None)])]);
    let context = snapshot.provider_context().as_provider_context();
    let TypeExpr::SchemaType { owned } = annotation(&snapshot) else {
        panic!("resolved annotation")
    };
    let mut missing = serde_json::to_value(&owned).unwrap();
    missing["owners"] = serde_json::to_value(crate::parser::NonEmptySchemaTypeOwners::singleton(
        SchemaTypeOwner::Provider("missing".into()),
    ))
    .unwrap();
    let missing = serde_json::from_value(missing).unwrap();
    assert!(matches!(
        context.custom_types.annotation_declaration(&missing),
        Err(CustomTypeValidationError::MissingDeclaration { .. })
    ));
    let mut partial = context.clone();
    partial.custom_types = context
        .custom_types
        .with_failed_providers(std::collections::BTreeSet::from(["p".into()]));
    let ty = crate::parser::parse_type_expr_str(&identity().to_string(), &partial).unwrap();
    let resolved = resolve_type_expr(&ty, &partial).unwrap();
    assert!(matches!(resolved, TypeExpr::UnloadedSchemaType(_)));
    assert!(matches!(
        partial.custom_types.annotation_declaration(&owned),
        Err(CustomTypeValidationError::ProvidersNotLoaded)
    ));
    let available = context
        .custom_types
        .with_failed_providers(std::collections::BTreeSet::from(["unrelated".into()]));
    assert_eq!(available.annotation_declaration(&owned).unwrap(), owned);
}

fn call_findings(
    source: &str,
    caller: &LoadedProviderSnapshot,
    sink: TypeExpr,
) -> Vec<ModuleCallFinding> {
    let context = caller.provider_context();
    let parsed = crate::parser::parse(source, context.as_provider_context()).unwrap();
    let signatures = std::collections::HashMap::from([(
        "child".into(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![crate::parser::ConsumerArgumentParameter {
                name: "x".into(),
                static_type: StaticType::Known(ConsumerResolvedType::new(sink)),
                default: None,
                description: None,
                validations: vec![],
            }],
            attributes: IndexMap::new(),
            requires: vec![],
        },
    )]);
    let schemas = caller.schema_registry();
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        schemas,
        context.as_provider_context(),
    );
    let report = check_bare_binding_uses(&ValidationTypeContext::new(
        configuration.judged(),
        context,
        &signatures,
    ));
    let bindings = BindingIndex::from_parsed_with_module_signatures(
        &parsed,
        schemas,
        &signatures,
        context.as_provider_context(),
    );
    deferred_types::with_session_for_test(context, |session| {
        check_module_calls(ModuleCallCheckInput {
            authored: &parsed,
            semantic: Some(&parsed),
            base_dir: None,
            imported_signatures: &signatures,
            provider_context: session,
            schemas,
            binding_use_report: &report,
            bindings: &bindings,
            reference_checks: ModuleCallReferenceChecks::Enabled,
        })
        .unwrap()
    })
}

#[test]
fn module_call_boundaries_check_schema_sources_and_forwarded_caller_arguments() {
    let callee = snapshot(vec![("p", vec![string(1, 64, None)])]);
    for width in [32, 64, 128] {
        let caller = snapshot(vec![("p", vec![string(1, width, None)])]);
        for source in [
            "let child = use { source = './child' }\nlet source = p.Values { value0 = 'VALID' }\nlet instance = child { x = source.value0 }\n",
            "arguments { x: aws.iam.Role.Name }\nlet child = use { source = './child' }\nlet instance = child { x = x }\n",
        ] {
            let findings = call_findings(source, &caller, annotation(&callee));
            assert!(
                findings.is_empty(),
                "identity admission at width {width}: {findings:#?}"
            );
        }
    }
}

#[test]
fn module_call_literal_values_obey_the_resolved_witness() {
    let caller = snapshot(vec![("p", vec![string(1, 128, None)])]);
    let callee = snapshot(vec![("p", vec![string(1, 64, None)])]);
    let source = format!(
        "let child = use {{ source = './child' }}\nlet instance = child {{ x = '{}' }}\n",
        "A".repeat(65)
    );
    let findings = call_findings(&source, &caller, annotation(&callee));
    assert!(
        findings.iter().any(|finding| finding
            .message()
            .to_string()
            .contains("length 65 is outside allowed range [1, 64]")),
        "{findings:#?}"
    );
}

#[test]
fn review_bare_enum_module_argument_rejects_empty_witness() {
    let enumeration = DraftType::enum_(
        identity(),
        Some(vec!["GOOD".into()]),
        vec![("GOOD".into(), "friendly".into())],
        None,
        None,
    );
    let caller = snapshot(vec![
        ("p", vec![enumeration.clone()]),
        ("q", vec![string(1, 128, None)]),
    ]);
    let callee = snapshot(vec![
        ("p", vec![enumeration, string(1, 4, None)]),
        ("q", vec![string(8, 12, None)]),
    ]);
    for (argument, sink) in [
        ("choice", annotation(&callee)),
        ("[choice]", TypeExpr::List(Box::new(annotation(&callee)))),
    ] {
        let source = format!(
            "let choice = aws.iam.Role.Name.friendly\n\
             let child = use {{ source = './child' }}\n\
             let instance = child {{ x = {argument} }}\n"
        );
        let findings = call_findings(&source, &caller, sink);
        assert!(
            findings
                .iter()
                .any(|finding| finding.message().to_string().contains("empty intersection")),
            "a bare enum must fail an empty witness: {argument}: {findings:#?}"
        );
    }
}

#[test]
fn review_bare_enum_module_argument_checks_retained_canonical_constraints() {
    let caller = snapshot(vec![(
        "p",
        vec![DraftType::enum_(
            identity(),
            Some(vec!["GOOD".into(), "invalid".into()]),
            vec![
                ("GOOD".into(), "long_alias".into()),
                ("invalid".into(), "bad_alias".into()),
            ],
            None,
            None,
        )],
    )]);
    for base in [
        string(4, 4, None),
        string(
            1,
            32,
            Some(crate::schema::PatternSpec::new(
                "^[A-Z]+$",
                crate::schema::PatternMatch::Full,
            )),
        ),
    ] {
        let callee = snapshot(vec![(
            "p",
            vec![DraftType::enum_with_base(
                identity(),
                base,
                None,
                vec![],
                None,
                None,
            )],
        )]);
        for (member, accepted) in [("long_alias", true), ("bad_alias", false)] {
            let source = format!(
                "let choice = aws.iam.Role.Name.{member}\n\
                 let child = use {{ source = './child' }}\n\
                 let instance = child {{ x = choice }}\n"
            );
            let findings = call_findings(&source, &caller, annotation(&callee));
            assert_eq!(findings.is_empty(), accepted, "{member}: {findings:#?}");
        }
    }
}

#[test]
fn unbounded_annotation_constraints_admit_explicitly_unbounded_schema_declarations() {
    for declarations in [
        vec![
            DraftType::refined_string(Some(identity()), None, None, None),
            DraftType::string_member(
                crate::schema::StringIdentityLayer::new(Some(identity()), None, None, None),
                crate::schema::StringMemberConstraints::new(None, Some((None, None))),
            ),
        ],
        vec![
            DraftType::refined_int(Some(identity()), None),
            DraftType::int_member(
                crate::schema::IntIdentityLayer::new(Some(identity()), None),
                crate::schema::IntMemberConstraints::new(Some((None, None))),
            ),
        ],
        vec![
            DraftType::refined_float(Some(identity()), None),
            DraftType::refined_float(Some(identity()), Some((None, None))),
        ],
    ] {
        let snapshot = snapshot(vec![("p", declarations)]);
        let lifted = lift_type_expr(&annotation(&snapshot)).unwrap();
        for index in [0, 1] {
            assert!(assign(
                &lifted,
                declaration(&snapshot, "p", index),
                &snapshot,
                "p"
            ));
            assert!(assign(
                declaration(&snapshot, "p", index),
                &lifted,
                &snapshot,
                "p"
            ));
        }
    }
}

#[test]
fn computed_access_keeps_only_identity_constraints() {
    let snapshot = snapshot(vec![(
        "p",
        vec![member(100, 128, None), member(1, 64, None)],
    )]);
    let parsed = crate::parser::parse(
        "let source = p.Values {}\nlet result = [([source.value0] ?? [source.value0])[0]]",
        snapshot.provider_context().as_provider_context(),
    )
    .unwrap();
    let report = crate::parser::retained::judge_expressions(
        &parsed,
        snapshot.schema_registry(),
        snapshot.provider_context().as_provider_context(),
    );
    let base = list(vec![Value::resource_ref("source", "value0", Vec::new())]);
    let access = crate::resource::apply_value_path_segment(
        Value::Deferred(crate::resource::DeferredValue::Coalesce {
            value: Box::new(base.clone()),
            default: Box::new(base),
        }),
        crate::resource::PathSegment::Subscript {
            index: crate::resource::Subscript::Int { index: 0 },
        },
        1,
        crate::resource::AccessMissPolicy::Error,
    )
    .unwrap()
    .into_value()
    .unwrap();
    assert!(matches!(
        access,
        Value::Deferred(crate::resource::DeferredValue::Access(_))
    ));
    let expected = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 1), "p");
    typecheck::check_type_expr_value(
        &TypeExpr::List(Box::new(expected)),
        &list(vec![access]),
        snapshot.provider_context(),
        report
            .binding(&crate::resource::RelativeBindingName::new("result"))
            .unwrap(),
    )
    .require_provider_schemas()
    .expect("a read's former member bounds are not value constraints");
}

#[test]
fn computed_access_rejects_heterogeneous_collection_in_validation() {
    let snapshot = snapshot(vec![(
        "p",
        vec![
            member(1, 64, None),
            member(1, 128, None),
            DraftType::list(member(1, 64, None)),
        ],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    for (fallback, expected_error) in [
        ("[source.value0, source.value1]", None),
        (
            "['ValidName', 1]",
            Some("collection element types disagree: `String` vs `Int`"),
        ),
        ("[source.value0, source.value0]", None),
    ] {
        let parsed = crate::parser::parse(
            &format!(
                "let source = p.Values {{}}\nlet sink = p.Values {{ value0 = (source.value2 ?? {fallback})[1] }}\n"
            ),
            context,
        )
        .expect("computed access fixture must parse");
        let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
        let result = validate_resource_ref_types(
            crate::config_loader::load_buffer_configuration(
                &parsed,
                snapshot.schema_registry(),
                context,
            )
            .judged(),
            &HashSet::new(),
            &bindings,
            &mut Vec::new(),
        );
        match expected_error {
            Some(expected) => {
                let error = result.expect_err("computed access must propagate inference failure");
                assert!(error.to_string().contains(expected), "{error}");
            }
            None => result.expect("a homogeneous bounded fallback must remain valid"),
        }
    }
}

#[test]
fn computed_access_rejects_heterogeneous_collection_in_typecheck() {
    let snapshot = snapshot(vec![("p", vec![member(1, 64, None), member(1, 128, None)])]);
    let expected = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 0), "p");
    let narrow = typed_function_result(expected.clone());
    let wide = typed_function_result(inference::attribute_type_to_type_expr(
        declaration(&snapshot, "p", 1),
        "p",
    ));
    for (index, (elements, expected_error)) in [
        (vec![narrow.clone(), wide], None),
        (
            vec![text("ValidName"), integer(1)],
            Some("collection element types disagree: `String` vs `Int`"),
        ),
        (vec![narrow.clone(), narrow.clone()], None),
    ]
    .into_iter()
    .enumerate()
    {
        let fallback = [
            "[source.value0, source.value1]",
            "['ValidName', 1]",
            "[source.value0, source.value0]",
        ][index];
        let parsed = crate::parser::parse(
            &format!("let source = p.Values {{}}\nlet result = ([source.value0, source.value0] ?? {fallback})[1]"),
            snapshot.provider_context().as_provider_context(),
        ).unwrap();
        let report = crate::parser::retained::judge_expressions(
            &parsed,
            snapshot.schema_registry(),
            snapshot.provider_context().as_provider_context(),
        );
        let access = crate::resource::apply_value_path_segment(
            Value::Deferred(crate::resource::DeferredValue::Coalesce {
                value: Box::new(list(vec![narrow.clone(), narrow.clone()])),
                default: Box::new(list(elements)),
            }),
            crate::resource::PathSegment::Subscript {
                index: crate::resource::Subscript::Int { index: 1 },
            },
            1,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap()
        .into_value()
        .unwrap();
        {
            let inference = report
                .binding(&crate::resource::RelativeBindingName::new("result"))
                .unwrap();
            let result = typecheck::check_type_expr_value(
                &expected,
                &access,
                snapshot.provider_context(),
                inference,
            )
            .require_provider_schemas();
            match expected_error {
                Some(expected) => {
                    let error =
                        result.expect_err("computed access must propagate inference failure");
                    assert!(matches!(
                        error,
                        typecheck::TypeExprValueError::InvalidAccess {
                            error: inference::InferenceError::HeterogeneousCollection { .. }
                        }
                    ));
                    assert_eq!(error.to_string(), expected);
                }
                None => result.expect("a homogeneous bounded fallback must remain valid"),
            }
        }
    }
}

#[test]
fn coalesce_and_collection_inference_share_the_identity_layer() {
    let snapshot = snapshot(vec![("p", vec![member(1, 64, None), member(1, 128, None)])]);
    let bindings = inference::InferenceBindings::from([(
        "source".into(),
        inference::InferenceBinding::Resource {
            provider: "p".into(),
            resource_type: "Values".into(),
        },
    )]);
    let first = Value::resource_ref("source", "value0", Vec::new());
    let second = Value::resource_ref("source", "value1", Vec::new());
    let canonical = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 0), "p");
    for (value, expected) in [
        (
            Value::Deferred(crate::resource::DeferredValue::Coalesce {
                value: Box::new(first.clone()),
                default: Box::new(second.clone()),
            }),
            canonical.clone(),
        ),
        (
            list(vec![first.clone(), second.clone()]),
            TypeExpr::List(Box::new(canonical.clone())),
        ),
        (
            map(IndexMap::from([("a".into(), first), ("b".into(), second)])),
            TypeExpr::Map(Box::new(canonical)),
        ),
    ] {
        let inferred = inference::infer_projected_type(
            &value,
            &bindings,
            snapshot.schema_registry(),
            snapshot.provider_context().as_provider_context(),
        )
        .unwrap();
        assert_eq!(inferred, expected);
    }
}

#[test]
fn enum_binding_mismatch_uses_the_typed_error_constructor() {
    let snapshot = snapshot(vec![(
        "p",
        vec![DraftType::enum_(
            identity(),
            Some(vec!["VALID".into()]),
            vec![],
            None,
            None,
        )],
    )]);
    let findings = call_findings(
        "arguments { x: Bool }\nlet child = use { source = './child' }\nlet instance = child { x = x }\n",
        &snapshot,
        annotation(&snapshot),
    );
    assert!(
        findings.iter().any(|finding| finding
            .message()
            .to_string()
            .contains("expected aws.iam.Role.Name, got Bool")),
        "{findings:#?}"
    );
}

fn typed_function_result(type_expr: TypeExpr) -> Value {
    let mut parsed = crate::parser::parse(
        "fn result() { 'VALID' }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let mut function = parsed.user_functions.remove("result").unwrap();
    function.return_type = Some(type_expr);
    Value::Deferred(crate::resource::DeferredValue::ResolvedFunctionCall {
        function: Arc::new(function),
        functions: Arc::new(HashMap::new()),
        args: Vec::new(),
        source_line: 1.into(),
    })
}

#[test]
fn enum_resource_binding_mismatch_keeps_the_exact_enum_name() {
    let snapshot = snapshot(vec![(
        "p",
        vec![DraftType::enum_(
            identity(),
            Some(vec!["VALID".into()]),
            vec![],
            None,
            None,
        )],
    )]);
    let parsed = crate::parser::parse(
        "arguments { x: Bool }\nlet target = p.Values { value0 = x }\n",
        snapshot.provider_context().as_provider_context(),
    )
    .unwrap();
    let signatures = HashMap::new();
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        snapshot.schema_registry(),
        snapshot.provider_context().as_provider_context(),
    );
    let report = check_bare_binding_uses(&ValidationTypeContext::new(
        configuration.judged(),
        snapshot.provider_context(),
        &signatures,
    ));
    assert!(
        report.findings().iter().any(|finding| finding
            .error
            .to_string()
            .contains("expected aws.iam.Role.Name, got Bool")),
        "{report:#?}"
    );

    let owner = crate::parser::SchemaTypeOwner::Provider("p".into());
    let target = ExpectedEnumTarget {
        attr_type: declaration(&snapshot, "p", 0),
        defs: crate::schema::empty_defs_for_schema_walks(),
        ancestry: FieldAncestry::WRITABLE,
        owner: &owner,
    };
    let error = binding_value_assignable_to_enum(
        &Value::Concrete(crate::resource::ConcreteValue::Bool(true)),
        Some(&TypeExpr::Bool),
        typecheck::TypecheckProviderContext::loaded(snapshot.provider_context()),
        &target,
        &EnumValueResolver::with_defs(target.attr_type, target.defs),
        &identity(),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Type mismatch: expected aws.iam.Role.Name, got Bool"
    );
}

#[test]
fn production_inference_compares_identity_only_value_types() {
    let snapshot = snapshot(vec![("p", vec![member(1, 64, None), member(1, 128, None)])]);
    let first = typed_function_result(inference::attribute_type_to_type_expr(
        declaration(&snapshot, "p", 0),
        "p",
    ));
    let second = typed_function_result(inference::attribute_type_to_type_expr(
        declaration(&snapshot, "p", 1),
        "p",
    ));
    let values = [
        Value::Deferred(crate::resource::DeferredValue::Coalesce {
            value: Box::new(first.clone()),
            default: Box::new(second.clone()),
        }),
        list(vec![first.clone(), second.clone()]),
        map(IndexMap::from([
            ("a".into(), first.clone()),
            ("b".into(), second.clone()),
        ])),
        Value::Deferred(crate::resource::DeferredValue::ModuleIf(Box::new(
            crate::parser::DeferredValueIf::new(
                crate::parser::DeferredIfCondition::new(
                    Value::Deferred(crate::resource::DeferredValue::Placeholder(
                        ParserPlaceholder::ModuleArgument {
                            name: "flag".into(),
                            diagnostic: Default::default(),
                        },
                    )),
                    "main.crn".into(),
                    1,
                ),
                first,
                second,
            ),
        ))),
    ];
    for value in values {
        let inferred = inference::infer_projected_type(
            &value,
            &inference::InferenceBindings::new(),
            snapshot.schema_registry(),
            snapshot.provider_context().as_provider_context(),
        )
        .unwrap();
        let canonical = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 0), "p");
        let expected = match &value {
            Value::Concrete(crate::resource::ConcreteValue::List(_)) => {
                TypeExpr::List(Box::new(canonical))
            }
            Value::Concrete(crate::resource::ConcreteValue::Map(_)) => {
                TypeExpr::Map(Box::new(canonical))
            }
            _ => canonical,
        };
        assert_eq!(inferred, expected);
    }
}

#[test]
fn every_boundary_admits_same_identity_member_restrictions() {
    let snapshot = snapshot(vec![("p", vec![member(1, 128, None), member(1, 64, None)])]);
    let context = snapshot.provider_context().as_provider_context();
    let mut parsed = crate::parser::parse(
        "let source = p.Values {}\nlet target = p.Values { value1 = source.value0 }\nattributes { name: aws.iam.Role.Name = source.value0 }\nexports { name: aws.iam.Role.Name = source.value0 }\nfn result(): aws.iam.Role.Name { source.value0 }\n",
        context,
    ).unwrap();
    deferred_types::with_session_for_test(snapshot.provider_context(), |session| {
        resolve_file_type_exprs_report(&mut parsed, session, None)
    })
    .unwrap();
    let narrow = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 1), "p");
    parsed.attribute_params[0].static_type = Some(narrow.clone());
    parsed.export_params[0].static_type = Some(narrow.clone());
    parsed.user_functions.get_mut("result").unwrap().return_type = Some(narrow);
    let parsed = inference::apply_inference(parsed, snapshot.schema_registry(), context);
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    validate_resource_ref_types(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            snapshot.schema_registry(),
            context,
        )
        .judged(),
        &HashSet::new(),
        &bindings,
        &mut Vec::new(),
    )
    .expect("same-identity resource reference defers its local constraints");
    validate_attribute_param_ref_types_with_bindings(&parsed.attribute_params, &bindings).unwrap();
    validate_export_param_ref_types_with_bindings(&parsed.export_params, &bindings).unwrap();
    validate_user_function_ref_types_with_bindings(&parsed, &bindings).unwrap();
}

#[test]
fn wider_name_resource_reference_discharge_matches_the_destination_literal_error() {
    use crate::binding_index::{BindingValueSource, ResolvedBindings};
    use crate::resource::knowledge::KnowledgeContext;
    use crate::validation::admission::{Candidate, admit};
    let snapshot = snapshot(vec![(
        "p",
        vec![member(100, 128, None), member(1, 64, None)],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    let parsed = crate::parser::parse_file(
        "let source = p.Values {}\nlet target = p.Values { value1 = source.value0 }\n",
        std::path::Path::new("main.crn"),
        context,
    )
    .unwrap()
    .resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    let destination = snapshot
        .schema_registry()
        .resource_attribute_destination(&parsed.resources[1].id, "value1", context)
        .unwrap();
    deferred_types::with_session_for_test(snapshot.provider_context(), |session| {
        let reference = crate::resource::Value::root_resource_ref("source", "value0", vec![]);
        let (candidate, boundary) = Candidate::validation_classified(
            reference
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification(),
            Some(&bindings),
            session,
        );
        let pending = admit(&destination, candidate, &boundary)
            .unwrap()
            .ready()
            .unwrap_err();
        let literal = crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
            "A".repeat(100).into(),
        ));
        let (candidate, boundary) = Candidate::validation_classified(
            literal
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification(),
            Some(&bindings),
            session,
        );
        let literal_error = admit(&destination, candidate, &boundary).unwrap_err();
        let mut resolved = ResolvedBindings::default();
        resolved.set(
            "source",
            HashMap::from([("value0".into(), literal)]),
            BindingValueSource::Local,
        );
        let error = pending.discharge(&resolved, session).unwrap_err();
        assert_eq!(error.resolution_error(), literal_error.resolution_error());
        assert_eq!(
            error
                .resolution_error()
                .to_string()
                .matches("value1:")
                .count(),
            1
        );
    });
}

#[test]
fn function_reads_ignore_slot_members_and_coalesce_keeps_destination_checks() {
    let snapshot = snapshot(vec![(
        "p",
        vec![member(100, 128, None), member(1, 64, None)],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    let mut parsed = crate::parser::parse(
        "attributes { name: aws.iam.Role.Name = 'VALID' }\n",
        context,
    )
    .unwrap();
    deferred_types::with_session_for_test(snapshot.provider_context(), |session| {
        resolve_file_type_exprs_report(&mut parsed, session, None)
    })
    .unwrap();
    let wide = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 0), "p");
    let narrow = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 1), "p");
    parsed.attribute_params[0].static_type = Some(narrow.clone());
    parsed.attribute_params[0].value = Some(crate::parser::BoundaryExpression::Value(
        typed_function_result(wide.clone()),
    ));
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    validate_user_function_ref_types_with_bindings(&parsed, &bindings)
        .expect("function value types contain identity bounds only");

    let value = Value::Deferred(crate::resource::DeferredValue::Coalesce {
        value: Box::new(typed_function_result(narrow)),
        default: Box::new(typed_function_result(wide)),
    });
    let evidence = crate::parser::parse(
        "let source = p.Values {}\nlet result = source.value1 ?? source.value0",
        context,
    )
    .unwrap();
    let report =
        crate::parser::retained::judge_expressions(&evidence, snapshot.schema_registry(), context);
    check_deferred_access_type(
        &value,
        declaration(&snapshot, "p", 1)
            .in_schema(crate::schema::empty_defs_for_schema_walks())
            .with_authority(
                &context.custom_types,
                crate::schema::SinkAuthority::Owner(&crate::parser::SchemaTypeOwner::Provider(
                    "p".into(),
                )),
            ),
        report
            .binding(&crate::resource::RelativeBindingName::new("result"))
            .unwrap(),
        snapshot.schema_registry(),
        &bindings,
    )
    .expect("the coalesce can select the fitting branch; the selected value remains checked at its destination");
}

#[test]
fn mismatch_formatter_preserves_refs_nested_types_and_map_key_constraints() {
    let snapshot = snapshot(vec![("p", vec![member(1, 64, None), member(1, 128, None)])]);
    let reference = AttributeType::ref_("Name");
    let expected_defs = BTreeMap::from([("Name".into(), declaration(&snapshot, "p", 0).clone())]);
    let actual_defs = BTreeMap::from([("Name".into(), declaration(&snapshot, "p", 1).clone())]);
    for root in [
        reference.clone(),
        AttributeType::list(reference.clone()),
        AttributeType::map(reference.clone()),
        AttributeType::map_with_key(reference.clone(), AttributeType::bool()),
        AttributeType::struct_(
            "Record",
            vec![crate::schema::CompiledStructField::required("name", reference).build()],
        ),
    ] {
        let error = TypeError::TypeMismatch {
            types: MismatchTypeNames::for_types(
                root.in_schema(&expected_defs),
                root.in_schema(&actual_defs),
            ),
        };
        let message = error.to_string();
        assert!(
            message.contains("aws.iam.Role.Name (length unbounded, member length 1..=64)"),
            "{message}"
        );
        assert!(
            message.contains("aws.iam.Role.Name (length unbounded, member length 1..=128)"),
            "{message}"
        );
    }
}

#[test]
fn integer_result_mismatches_share_names_in_value_and_resource_checkers() {
    let snapshot = snapshot(vec![(
        "p",
        vec![DraftType::refined_int(
            Some(TypeIdentity::bare("Int")),
            Some((Some(1), Some(64))),
        )],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    let parsed = crate::parser::parse(
        "arguments { x: Int }\nlet target = p.Values { value0 = x + 1 }\n",
        context,
    )
    .unwrap();
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    let expected = "expected Int (range 1..=64), got Int (range unbounded)";
    let error = validate_resource_ref_types(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            snapshot.schema_registry(),
            context,
        )
        .judged(),
        &HashSet::new(),
        &bindings,
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");

    let evidence =
        crate::parser::parse("arguments { x: Int }\nlet result = x + 1", context).unwrap();
    let report =
        crate::parser::retained::judge_expressions(&evidence, snapshot.schema_registry(), context);
    let error = typecheck::check_type_expr_value(
        &inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 0), "p"),
        &parsed.resources[0].attributes["value0"],
        snapshot.provider_context(),
        report
            .binding(&crate::resource::RelativeBindingName::new("result"))
            .unwrap(),
    )
    .require_provider_schemas()
    .unwrap_err();
    assert_eq!(error.to_string(), expected);
}

#[test]
fn integer_operand_mismatches_do_not_hide_a_named_strings_constraints() {
    let snapshot = snapshot(vec![(
        "p",
        vec![DraftType::refined_string(
            Some(TypeIdentity::bare("Int")),
            None,
            Some((Some(1), Some(16))),
            None,
        )],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    let mut parsed = crate::parser::parse(
        "let source = p.Values {}\nlet result = source.value0 + 1\n",
        context,
    )
    .unwrap();
    let error = inference::infer_projected_type(
        &parsed.variables["result"],
        &inference::bindings_from_parsed(&parsed),
        snapshot.schema_registry(),
        context,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "operator '+': left operand must be Int, got Int (length 1..=16)"
    );
    let crate::resource::Value::Deferred(crate::resource::DeferredValue::IntegerArithmetic(
        expression,
    )) = &parsed.variables["result"]
    else {
        panic!("deferred arithmetic")
    };
    let computed = crate::integer_arithmetic::evaluate_integer_values(
        typed_function_result(inference::attribute_type_to_type_expr(
            declaration(&snapshot, "p", 0),
            "p",
        )),
        crate::integer_arithmetic::IntegerOp::Add,
        integer(1),
        expression.operator_span(),
    )
    .unwrap();
    parsed.variables.insert("result".into(), computed);
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    let error = validate_resource_ref_types(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            snapshot.schema_registry(),
            context,
        )
        .judged(),
        &HashSet::new(),
        &bindings,
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("left operand must be Int, got Int (length 1..=16)"),
        "{error}"
    );
}

#[test]
fn handle_mismatches_use_the_same_type_formatter_and_preserve_handle_wording() {
    let snapshot = snapshot(vec![(
        "p",
        vec![DraftType::refined_string(
            Some(TypeIdentity::new(Some("p"), Vec::<String>::new(), "Values")),
            None,
            Some((Some(1), Some(128))),
            None,
        )],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    let parsed = crate::parser::parse("let source = p.Values {}\n", context).unwrap();
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    let path = crate::resource::AccessPath::new("source", "value0");
    let error = check_ref_type_for_handle_sink(
        bindings.ref_type(&path),
        &ResourceTypePath::new("p", "Values"),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "expected resource handle p.Values; got p.Values (length 1..=128)"
    );
    let error = HandleSinkError::expanded_value_for_handle(
        &ResourceTypePath::new("p", "Values"),
        &Value::resource_ref("source", "value0", Vec::new()),
        &bindings,
    );
    assert_eq!(
        error.to_string(),
        "expected resource handle p.Values; got p.Values (length 1..=128)"
    );
    let error = crate::resolved_graph::ExpandedGraphResolutionError::HandleTypeMismatch {
        owner: "output".into(),
        reference: "source".into(),
        types: MismatchTypeNames::for_resource_types(
            &ResourceTypePath::new("p", "Expected"),
            &ResourceTypePath::new("p", "Values"),
        ),
    };
    assert_eq!(
        error.to_string(),
        "output: expected resource handle p.Expected; got resource handle p.Values from 'source'"
    );
}

// #4282: builtin construction evidence must reach the same named field and
// resolved-value checker as an equivalent mixed interpolation.
fn constructed_builtin_at_named_field(expression: &str, sink: DraftType) -> Result<(), String> {
    let loaded = snapshot(vec![("p", vec![sink])]);
    let runtime = ProviderContext {
        decryptor: Some(Arc::new(|_, _| Ok("abc".into()))),
        ..ProviderContext::new(crate::EnvironmentSnapshot::new([(
            "ISSUE_4282".into(),
            "abc".into(),
        )]))
    };
    let context = loaded.provider_context().with_runtime(&runtime);
    let mut parsed = crate::parser::parse(
        &format!("let result = {expression}\nlet target = p.Values {{ value0 = result }}"),
        context.as_provider_context(),
    )
    .unwrap();
    crate::parser::resolve_resource_refs_with_config(&mut parsed, context.as_provider_context())
        .unwrap();
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        loaded.schema_registry(),
        context.as_provider_context(),
    );
    let signatures = HashMap::new();
    let uses = check_bare_binding_uses(&ValidationTypeContext::new(
        configuration.judged(),
        &context,
        &signatures,
    ));
    let expected = inference::attribute_type_to_type_expr(declaration(&loaded, "p", 0), "p");
    typecheck::check_type_expr_value(
        &expected,
        &parsed.variables["result"],
        &context,
        configuration
            .judged()
            .report()
            .binding(&crate::resource::RelativeBindingName::new("result"))
            .unwrap(),
    )
    .require_provider_schemas()
    .map_err(|error| error.to_string())?;
    match uses.findings().first() {
        Some(finding) => Err(finding.error.to_string()),
        None => Ok(()),
    }
}

fn assert_constructed_builtin_at_named_field(expression: &str, value: &str) {
    let interpolation = format!(r#""${{''}}{value}""#);
    for declaration in [
        string(1, 32, None),
        string(
            1,
            32,
            Some(crate::schema::PatternSpec::new(
                "^never$",
                crate::schema::PatternMatch::Full,
            )),
        ),
        string(1, 1, None),
    ] {
        let expected = constructed_builtin_at_named_field(&interpolation, declaration.clone());
        let actual = constructed_builtin_at_named_field(expression, declaration);
        assert_eq!(actual, expected, "{expression}");
    }
    assert!(constructed_builtin_at_named_field(expression, string(1, 32, None)).is_ok());
}

#[test]
fn issue_4282_upper_at_named_string_uses_interpolation_rules() {
    assert_constructed_builtin_at_named_field("upper('abc')", "ABC");
}

#[test]
fn issue_4282_lower_at_named_string_uses_interpolation_rules() {
    assert_constructed_builtin_at_named_field("lower('ABC')", "abc");
}

#[test]
fn issue_4282_replace_at_named_string_uses_interpolation_rules() {
    assert_constructed_builtin_at_named_field("replace('X', 'b', 'aXc')", "abc");
}

#[test]
fn issue_4282_trim_at_named_string_uses_interpolation_rules() {
    assert_constructed_builtin_at_named_field("trim(' abc ')", "abc");
}

#[test]
fn issue_4282_cidr_subnet_at_named_string_uses_interpolation_rules() {
    assert_constructed_builtin_at_named_field("cidr_subnet('10.0.0.0/16', 4, 1)", "10.0.16.0/20");
}

#[test]
fn issue_4282_split_checks_each_named_string_element() {
    for (input, accepted) in [
        ("abc,def", true),
        ("abc,TOOLONG", false),
        ("abc,123", false),
    ] {
        let declaration = DraftType::list(string(
            1,
            3,
            Some(crate::schema::PatternSpec::new(
                "^[a-z]+$",
                crate::schema::PatternMatch::Full,
            )),
        ));
        let actual = constructed_builtin_at_named_field(
            &format!("split(',', '{input}')"),
            declaration.clone(),
        );
        let elements = input
            .split(',')
            .map(|value| format!(r#""${{''}}{value}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let expected = constructed_builtin_at_named_field(&format!("[{elements}]"), declaration);
        assert_eq!(actual, expected, "{input}");
        assert_eq!(actual.is_ok(), accepted, "{input}: {actual:?}");
    }
}

#[test]
fn issue_4282_external_strings_and_keys_keep_named_sink_rejection() {
    let loaded = snapshot(vec![("p", vec![string(1, 32, None)])]);
    for expression in ["env('ISSUE_4282')", "decrypt('ciphertext')"] {
        assert!(constructed_builtin_at_named_field(expression, string(1, 32, None)).is_err());
        let parsed = crate::parser::parse(
            &format!("let result = {expression}"),
            loaded.provider_context().as_provider_context(),
        )
        .unwrap();
        let report = crate::parser::retained::judge_expressions(
            &parsed,
            loaded.schema_registry(),
            loaded.provider_context().as_provider_context(),
        );
        let error = crate::parser::retained::admit_expression(
            report
                .binding(&crate::resource::RelativeBindingName::new("result"))
                .unwrap(),
            &annotation(&loaded),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "expected aws.iam.Role.Name, got String");
    }
    assert!(
        constructed_builtin_at_named_field(
            "keys({ abc = 1 })",
            DraftType::list(string(1, 32, None)),
        )
        .is_err()
    );
}

#[test]
fn issue_4282_secret_join_preserves_construction_and_masks_errors() {
    let marker = "PRIVATE-4282";
    assert!(
        constructed_builtin_at_named_field(
            "secret(join('', ['valid', '-suffix']))",
            string(1, 32, None),
        )
        .is_ok()
    );
    for declaration in [string(1, 32, None), string(1, 3, None)] {
        let actual = constructed_builtin_at_named_field(
            &format!("secret(join('', ['{marker}', '-suffix']))"),
            declaration.clone(),
        );
        let expected = constructed_builtin_at_named_field(
            &format!(r#"secret("{marker}-${{'suffix'}}")"#),
            declaration,
        );
        assert_eq!(actual, expected);
        if let Err(error) = actual {
            assert!(!error.contains(marker), "{error}");
            assert_eq!(error, "expected aws.iam.Role.Name, got secret");
        }
    }
}

#[test]
fn issue_4282_review_split_coalesce_checks_literal_alternative() {
    let loaded = snapshot(vec![(
        "p",
        vec![
            DraftType::string(),
            DraftType::list(string(
                1,
                32,
                Some(crate::schema::PatternSpec::new(
                    "^[a-z]+$",
                    crate::schema::PatternMatch::Full,
                )),
            )),
        ],
    )]);
    let context = loaded.provider_context();
    let parsed = crate::parser::parse(
        "let src = p.Values {}\nlet r = split(',', src.value0) ?? ['UPPER']\n",
        context.as_provider_context(),
    )
    .unwrap();
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        loaded.schema_registry(),
        context.as_provider_context(),
    );
    let expected = inference::attribute_type_to_type_expr(declaration(&loaded, "p", 1), "p");
    let checked = typecheck::check_type_expr_value(
        &expected,
        &parsed.variables["r"],
        context,
        configuration
            .judged()
            .report()
            .binding(&crate::resource::RelativeBindingName::new("r"))
            .unwrap(),
    )
    .require_provider_schemas();
    let error = checked.expect_err("the literal alternative must retain its value check");
    assert_eq!(
        error.to_string(),
        "at list element 0: expected aws.iam.Role.Name, got string 'UPPER': p rejected `aws.iam.Role.Name`: Invalid Name value 'UPPER': does not match required pattern /^[a-z]+$/ (Full)"
    );
}

#[test]
fn member_constraints_do_not_enter_multi_owner_identity_annotations() {
    use crate::schema::{PatternMatch, PatternSpec, StringIdentityLayer, StringMemberConstraints};
    let identity_layer = || {
        StringIdentityLayer::new(
            Some(identity()),
            Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
            Some((Some(1), Some(32))),
            None,
        )
    };
    let a = DraftType::string_member(
        identity_layer(),
        StringMemberConstraints::new(
            Some(PatternSpec::new("a.*", PatternMatch::Full)),
            Some((Some(5), Some(16))),
        ),
    );
    let b = DraftType::string_member(
        identity_layer(),
        StringMemberConstraints::new(
            Some(PatternSpec::new("b.*", PatternMatch::Full)),
            Some((Some(3), Some(20))),
        ),
    );
    let loaded = snapshot(vec![("a", vec![a]), ("b", vec![b])]);
    let ty = annotation(&loaded);
    let TypeExpr::SchemaType { owned } = &ty else {
        panic!("owned annotation")
    };
    assert_eq!(owned.owners().iter().len(), 2);
    let wire = serde_json::to_value(&ty).unwrap();
    let scalar = &wire["SchemaType"]["owned"]["constraints"]["scalar"]["String"];
    assert!(scalar.get("member_layer").is_none(), "{scalar}");
    let restored: TypeExpr = serde_json::from_value(wire).unwrap();
    assert_eq!(restored, ty);
    let lifted = lift_type_expr(&restored).unwrap();
    assert!(lifted.validate(&text("ccc")).is_ok());
    assert!(declaration(&loaded, "a", 0).validate(&text("ccc")).is_err());
    assert!(declaration(&loaded, "b", 0).validate(&text("ccc")).is_err());
}

#[test]
fn owners_conjoin_differing_identity_layers_without_member_constraints() {
    use crate::schema::{PatternMatch, PatternSpec};
    let loaded = snapshot(vec![
        (
            "a",
            vec![string(
                1,
                16,
                Some(PatternSpec::new("a.*", PatternMatch::Full)),
            )],
        ),
        (
            "b",
            vec![string(
                3,
                32,
                Some(PatternSpec::new(".*z", PatternMatch::Full)),
            )],
        ),
    ]);
    let ty = annotation(&loaded);
    let lifted = lift_type_expr(&ty).unwrap();
    for (value, valid) in [("abz", true), ("az", false), ("xbz", false), ("abc", false)] {
        assert_eq!(lifted.validate(&text(value)).is_ok(), valid, "{value}");
    }
    let restored: TypeExpr = serde_json::from_value(serde_json::to_value(&ty).unwrap()).unwrap();
    assert_eq!(restored, ty);
    let restored = lift_type_expr(&restored).unwrap();
    assert!(restored.validate(&text("abz")).is_ok());
    assert!(restored.validate(&text("abc")).is_err());
    let mut tampered = serde_json::to_value(&ty).unwrap();
    tampered["SchemaType"]["owned"]["constraints"]["scalar"]["String"]["identity_layer"]["length"]
        ["min"] = serde_json::json!({"Inclusive": 0});
    assert!(
        serde_json::from_value::<TypeExpr>(tampered).is_err(),
        "owner bounds cannot disappear from retained evidence"
    );

    let integers = snapshot(vec![
        (
            "a",
            vec![DraftType::refined_int(
                Some(identity()),
                Some((Some(1), Some(20))),
            )],
        ),
        (
            "b",
            vec![DraftType::refined_int(
                Some(identity()),
                Some((Some(5), Some(30))),
            )],
        ),
    ]);
    let integers = lift_type_expr(&annotation(&integers)).unwrap();
    for (value, valid) in [(3, false), (5, true), (20, true), (21, false)] {
        assert_eq!(integers.validate(&integer(value)).is_ok(), valid, "{value}");
    }

    let disjoint = snapshot(vec![
        ("a", vec![string(1, 2, None)]),
        ("b", vec![string(4, 6, None)]),
    ]);
    let disjoint = lift_type_expr(&annotation(&disjoint)).unwrap();
    let error = disjoint.validate(&text("abc")).unwrap_err().to_string();
    assert!(
        error.contains("empty intersection of scalar constraints"),
        "{error}"
    );
}

#[test]
fn every_annotation_sink_admits_identity_before_member_containment() {
    let snapshot = snapshot(vec![("p", vec![member(1, 128, None), member(1, 64, None)])]);
    let context = snapshot.provider_context().as_provider_context();
    let mut parsed = crate::parser::parse(
        "let source = p.Values {}\nattributes { name: aws.iam.Role.Name = source.value0 }\nexports { name: aws.iam.Role.Name = source.value0 }\nfn result(): aws.iam.Role.Name { source.value0 }\nfn take(x: aws.iam.Role.Name) { x }\nlet accepted = take(source.value0)\n", context,
    ).unwrap();
    deferred_types::with_session_for_test(snapshot.provider_context(), |session| {
        resolve_file_type_exprs_report(&mut parsed, session, None)
    })
    .unwrap();
    let parsed = inference::apply_inference(parsed, snapshot.schema_registry(), context);
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    validate_attribute_param_ref_types_with_bindings(&parsed.attribute_params, &bindings)
        .expect("attribute member must defer");
    validate_export_param_ref_types_with_bindings(&parsed.export_params, &bindings)
        .expect("export member must defer");
    validate_user_function_ref_types_with_bindings(&parsed, &bindings)
        .expect("function members must defer");
    let bounded = super::named_annotation_tests::snapshot(vec![("p", vec![string(1, 64, None)])]);
    let narrow = annotation(&bounded);
    let mut functions = crate::parser::parse(
        &format!(
            "fn take(x: aws.iam.Role.Name) {{ x }}\nfn result(): aws.iam.Role.Name {{ '{}' }}\n",
            "A".repeat(80)
        ),
        context,
    )
    .unwrap()
    .user_functions;
    functions.get_mut("take").unwrap().params[0].param_type = Some(narrow.clone());
    functions.get_mut("result").unwrap().return_type = Some(narrow);
    deferred_types::with_session_for_test(bounded.provider_context(), |session| {
        let call = |name: &str,
                    args: Vec<
            crate::resource::FunctionArgument<ParserPlaceholder, RelativeBindingName>,
        >| {
            Value::Deferred(crate::resource::DeferredValue::ResolvedFunctionCall {
                function: Arc::new(functions[name].clone()),
                functions: Arc::new(functions.clone()),
                args,
                source_line: 1.into(),
            })
        };
        let resolved = crate::binding_index::ResolvedBindings::default();
        for (length, valid) in [(20, true), (80, false)] {
            let value = call(
                "take",
                vec![crate::resource::FunctionArgument::from_value(text(
                    "A".repeat(length),
                ))],
            );
            let result = crate::resolver::resolve_ref_value(&value, &resolved, session);
            assert_eq!(result.is_ok(), valid, "{result:?}");
            if let Err(error) = result {
                assert_eq!(
                    error.to_string().matches("outside allowed range").count(),
                    1,
                    "{error}"
                );
            }
        }
        let error = crate::resolver::resolve_ref_value(&call("result", vec![]), &resolved, session)
            .unwrap_err();
        assert_eq!(
            error.to_string().matches("outside allowed range").count(),
            1,
            "{error}"
        );
    });
}

#[test]
fn retained_read_types_do_not_claim_slot_member_bounds() {
    let snapshot = snapshot(vec![(
        "p",
        vec![member(100, 128, None), member(1, 64, None)],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    let parsed = crate::parser::parse(
        "let source = p.Values {}\nlet value = source.value0\n",
        context,
    )
    .unwrap();
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        snapshot.schema_registry(),
        context,
    );
    let expression = configuration
        .judged()
        .report()
        .binding(&crate::resource::RelativeBindingName::new("value"))
        .unwrap();
    let sink = inference::attribute_type_to_type_expr(declaration(&snapshot, "p", 1), "p");
    crate::parser::retained::admit_with_context(
        expression,
        &sink,
        crate::parser::retained::AdmissionContext::Assignment,
        context,
    )
    .expect("only an actual destination slot owns member bounds");
}

#[test]
fn owner_conjunction_wire_contains_only_identity_records() {
    let loaded = snapshot(vec![
        ("p", vec![string(1, 16, None)]),
        ("q", vec![string(3, 32, None)]),
    ]);
    let ty = annotation(&loaded);
    let wire = serde_json::to_value(&ty).unwrap();
    let owners = wire["SchemaType"]["owned"]["constraints"]["owner_layers"]
        .as_object()
        .unwrap();
    assert_eq!(
        owners.keys().map(String::as_str).collect::<Vec<_>>(),
        ["p", "q"]
    );
    for layer in owners.values() {
        let owner = &layer["String"]["identity_layer"];
        assert!(owner.get("member_layer").is_none(), "{owner}");
        assert!(
            owner.get("pattern").is_some() && owner.get("length").is_some(),
            "{owner}"
        );
    }
    let restored: TypeExpr = serde_json::from_value(wire).unwrap();
    assert_eq!(restored, ty);
}

#[test]
fn layered_scalar_evidence_fixture_round_trips_and_rebinds() {
    use crate::schema::{
        IntIdentityLayer, IntMemberConstraints, PatternMatch, PatternSpec, StringIdentityLayer,
        StringMemberConstraints,
    };
    let declarations = snapshot(vec![(
        "p",
        vec![
            DraftType::string_member(
                StringIdentityLayer::new(
                    Some(identity()),
                    Some(PatternSpec::new("[A-Z]+", PatternMatch::Full)),
                    Some((Some(1), Some(32))),
                    None,
                ),
                StringMemberConstraints::new(None, Some((Some(2), Some(4)))),
            ),
            DraftType::int_member(
                IntIdentityLayer::new(
                    Some(TypeIdentity::from_dotted("aws.Count")),
                    Some((Some(0), Some(100))),
                ),
                IntMemberConstraints::new(Some((Some(1), Some(10)))),
            ),
        ],
    )]);
    let owners = snapshot(vec![
        (
            "p",
            vec![string(
                1,
                16,
                Some(PatternSpec::new("[A-Z]+", PatternMatch::Full)),
            )],
        ),
        (
            "q",
            vec![string(
                3,
                32,
                Some(PatternSpec::new("A.*", PatternMatch::Full)),
            )],
        ),
    ]);
    let fixture = BTreeMap::from([
        (
            "string".to_owned(),
            inference::attribute_type_to_type_expr(declaration(&declarations, "p", 0), "p"),
        ),
        (
            "int".to_owned(),
            inference::attribute_type_to_type_expr(declaration(&declarations, "p", 1), "p"),
        ),
        ("owners".to_owned(), annotation(&owners)),
    ]);
    let json = include_str!("fixtures/layered_scalar_evidence.json");
    assert_eq!(crate::utils::pretty_with_newline(&fixture).unwrap(), json);
    let restored: BTreeMap<String, TypeExpr> = serde_json::from_str(json).unwrap();
    assert_eq!(fixture, restored);
    for (key, snapshot, values) in [
        (
            "string",
            &declarations,
            vec![(text("AB"), true), (text("ab"), false), (text("A"), true)],
        ),
        (
            "int",
            &declarations,
            vec![
                (integer(5), true),
                (integer(-1), false),
                (integer(11), true),
            ],
        ),
        (
            "owners",
            &owners,
            vec![
                (text("ABC"), true),
                (text("ZZZ"), false),
                (text("Abc"), false),
            ],
        ),
    ] {
        for (value, valid) in values {
            let result = typecheck::check_type_expr_value(
                &restored[key],
                &value,
                snapshot.provider_context(),
                &crate::parser::retained::value_dependent_judgement_for_test(),
            )
            .require_provider_schemas();
            assert_eq!(result.is_ok(), valid, "{key}: {result:?}");
        }
    }
}

#[test]
fn value_type_records_reject_member_constraints_in_both_origins() {
    let fixture: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/layered_scalar_evidence.json")).unwrap();
    for key in ["string", "int"] {
        for origin in ["Projection", "Annotation"] {
            let mut tampered = fixture[key].clone();
            let owned = &mut tampered["SchemaType"]["owned"];
            owned["origin"] = serde_json::json!(origin);
            if origin == "Annotation" {
                owned["projection_fingerprint"] = serde_json::Value::Null;
            }
            if key == "string" {
                owned["constraints"]["scalar"]["String"]["member_layer"] = serde_json::json!({"pattern":null,"length":{"min":"Unbounded","max":"Unbounded"}});
            } else {
                owned["constraints"]["scalar"]["Int"]["member_range"] =
                    serde_json::json!({"min":"Unbounded","max":"Unbounded"});
            }
            assert!(
                serde_json::from_value::<TypeExpr>(tampered).is_err(),
                "{origin} retained {key} members"
            );
        }
    }
}

#[test]
fn annotation_projection_equality_matches_wire_equality() {
    let loaded = snapshot(vec![("p", vec![string(1, 32, None)])]);
    let annotation = annotation(&loaded);
    let projection = inference::attribute_type_to_type_expr(declaration(&loaded, "p", 0), "p");
    assert_eq!(
        annotation == projection,
        serde_json::to_value(&annotation).unwrap() == serde_json::to_value(&projection).unwrap()
    );
    let mut changed = serde_json::to_value(&projection).unwrap();
    changed["SchemaType"]["owned"]["projection_fingerprint"] = serde_json::json!("0".repeat(64));
    let changed: TypeExpr = serde_json::from_value(changed).unwrap();
    assert_ne!(
        projection, changed,
        "fingerprints are portable equality evidence"
    );
}

#[test]
fn annotation_owner_layers_must_match_the_owner_set() {
    let loaded = snapshot(vec![
        ("a", vec![string(1, 32, None)]),
        ("b", vec![string(3, 64, None)]),
    ]);
    let wire = serde_json::to_value(annotation(&loaded)).unwrap();
    for owners in [
        serde_json::json!([{"Provider":"a"}]),
        serde_json::json!([{"Provider":"a"},{"Provider":"b"},{"Provider":"extra"}]),
    ] {
        let mut tampered = wire.clone();
        tampered["SchemaType"]["owned"]["owners"] = owners;
        assert!(
            serde_json::from_value::<TypeExpr>(tampered).is_err(),
            "owner layers cannot be extra or missing"
        );
    }
    let layer =
        serde_json::to_string(&wire["SchemaType"]["owned"]["constraints"]["owner_layers"]["a"])
            .unwrap();
    let duplicate = serde_json::to_string(&wire).unwrap().replace(
        "\"owner_layers\":{",
        &format!("\"owner_layers\":{{\"a\":{layer},"),
    );
    assert!(
        serde_json::from_str::<TypeExpr>(&duplicate).is_err(),
        "duplicate owner entries are invalid"
    );
}

#[test]
fn single_owner_annotations_have_one_canonical_layer_form() {
    let loaded = snapshot(vec![("a", vec![string(1, 32, None)])]);
    let annotation = annotation(&loaded);
    let wire = serde_json::to_value(&annotation).unwrap();
    let identity =
        wire["SchemaType"]["owned"]["constraints"]["scalar"]["String"]["identity_layer"].clone();
    for layers in [
        serde_json::json!([identity.clone()]),
        serde_json::json!({"a":{"String":{"identity_layer": identity}}}),
    ] {
        let mut tampered = wire.clone();
        tampered["SchemaType"]["owned"]["constraints"]["owner_layers"] = layers;
        assert!(
            serde_json::from_value::<TypeExpr>(tampered).is_err(),
            "single-owner annotations omit owner_layers"
        );
    }
    let restored: TypeExpr = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(restored, annotation);
    assert_eq!(serde_json::to_value(restored).unwrap(), wire);
}

#[test]
fn projection_constraint_records_reject_unknown_fields() {
    let fixture: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/layered_scalar_evidence.json")).unwrap();
    for key in ["string", "int"] {
        for extra in ["owner_layers", "future_constraint"] {
            let mut tampered = fixture[key].clone();
            tampered["SchemaType"]["owned"]["constraints"][extra] = serde_json::json!([]);
            assert!(
                serde_json::from_value::<TypeExpr>(tampered).is_err(),
                "projection accepted {extra}"
            );
        }
    }
}

#[test]
fn enum_identity_annotations_reject_nonmembers_matching_the_base() {
    let loaded = snapshot(vec![(
        "p",
        vec![DraftType::enum_(
            identity(),
            Some(vec!["GOOD".into()]),
            vec![],
            None,
            None,
        )],
    )]);
    let annotation = annotation(&loaded);
    let restored: TypeExpr =
        serde_json::from_value(serde_json::to_value(&annotation).unwrap()).unwrap();
    for ty in [&annotation, &restored] {
        // Main's materialized scalar view is the enum base; contextual
        // annotation validation also checks the enum declaration's values.
        assert!(lift_type_expr(ty).unwrap().validate(&text("OTHER")).is_ok());
        for (value, valid) in [
            (
                Value::Concrete(crate::resource::ConcreteValue::CanonicalEnum(
                    crate::resource::CanonicalEnumValue::new_for_test(identity(), "GOOD"),
                )),
                true,
            ),
            (text("OTHER"), false),
            (
                Value::Concrete(crate::resource::ConcreteValue::EnumIdentifier(
                    crate::resource::RawEnumIdentifier::parse(format!("{}.OTHER", identity())),
                )),
                false,
            ),
        ] {
            let result = typecheck::check_type_expr_value(
                ty,
                &value,
                loaded.provider_context(),
                &crate::parser::retained::value_dependent_judgement_for_test(),
            )
            .require_provider_schemas();
            assert_eq!(result.is_ok(), valid, "{value:?}: {result:?}");
        }
    }
}

#[test]
fn annotation_records_accept_empty_bounds_with_consistent_evidence() {
    for (first, second, kind, bounds) in [
        (string(1, 4, None), string(8, 12, None), "String", "length"),
        (
            DraftType::refined_int(Some(identity()), Some((Some(1), Some(4)))),
            DraftType::refined_int(Some(identity()), Some((Some(8), Some(12)))),
            "Int",
            "identity_range",
        ),
        (
            DraftType::refined_float(
                Some(identity()),
                Some((Some(crate::finite(1.0)), Some(crate::finite(4.0)))),
            ),
            DraftType::refined_float(
                Some(identity()),
                Some((Some(crate::finite(8.0)), Some(crate::finite(12.0)))),
            ),
            "Float",
            "range",
        ),
    ] {
        let single = snapshot(vec![("a", vec![first.clone()])]);
        let mut wire = serde_json::to_value(annotation(&single)).unwrap();
        let scalar = &mut wire["SchemaType"]["owned"]["constraints"]["scalar"][kind];
        let scalar = if kind == "String" {
            &mut scalar["identity_layer"]
        } else {
            scalar
        };
        scalar[bounds]["min"] = serde_json::json!({"Inclusive":5});
        wire["SchemaType"]["owned"]["constraints"]["empty_intersection"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<TypeExpr>(wire.clone()).is_ok(),
            "{kind}: a same-owner conjunction may be empty"
        );
        wire["SchemaType"]["owned"]["constraints"]["empty_intersection"] = serde_json::json!(false);
        assert!(serde_json::from_value::<TypeExpr>(wire).is_err());

        let loaded = snapshot(vec![("a", vec![first]), ("b", vec![second])]);
        let annotation = annotation(&loaded);
        let mut wire = serde_json::to_value(&annotation).unwrap();
        assert!(
            serde_json::from_value::<TypeExpr>(wire.clone()).is_ok(),
            "nonempty owners may have an empty aggregate"
        );
        let scalar = &mut wire["SchemaType"]["owned"]["constraints"]["owner_layers"]["a"][kind];
        let scalar = if kind == "String" {
            &mut scalar["identity_layer"]
        } else {
            scalar
        };
        scalar[bounds]["min"] = serde_json::json!({"Inclusive":5});
        assert!(
            serde_json::from_value::<TypeExpr>(wire.clone()).is_ok(),
            "{kind}: an empty owner layer agrees with this aggregate"
        );
        wire["SchemaType"]["owned"]["constraints"]["empty_intersection"] = serde_json::json!(false);
        assert!(serde_json::from_value::<TypeExpr>(wire).is_err());
    }
}

#[test]
fn within_owner_disjoint_float_annotation_round_trips() {
    let loaded = snapshot(vec![(
        "p",
        vec![
            DraftType::refined_float(
                Some(identity()),
                Some((Some(crate::finite(1.0)), Some(crate::finite(4.0)))),
            ),
            DraftType::refined_float(
                Some(identity()),
                Some((Some(crate::finite(8.0)), Some(crate::finite(12.0)))),
            ),
        ],
    )]);
    let original = annotation(&loaded);
    let TypeExpr::SchemaType { owned } = &original else {
        panic!("annotation")
    };
    assert!(owned.has_empty_intersection());
    let decoded: TypeExpr =
        serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn decoded_annotation_checks_current_owner_declarations() {
    let old = snapshot(vec![("p", vec![string(1, 128, None)])]);
    let current = snapshot(vec![(
        "p",
        vec![string(
            1,
            32,
            Some(crate::schema::PatternSpec::new(
                "[a-z]+",
                crate::schema::PatternMatch::Full,
            )),
        )],
    )]);
    let decoded: TypeExpr =
        serde_json::from_value(serde_json::to_value(annotation(&old)).unwrap()).unwrap();
    for (value, valid) in [("valid", true), ("UPPER", false)] {
        let checked = typecheck::check_type_expr_value(
            &decoded,
            &text(value),
            current.provider_context(),
            &crate::parser::retained::value_dependent_judgement_for_test(),
        )
        .require_provider_schemas();
        assert_eq!(checked.is_ok(), valid, "{value}: {checked:?}");
    }
}

#[test]
fn owner_constraint_conjoin_promotes_and_preserves_typed_owner_maps() {
    let a = snapshot(vec![("a", vec![string(1, 32, None)])]);
    let b = snapshot(vec![("b", vec![string(3, 64, None)])]);
    let c = snapshot(vec![("c", vec![string(5, 128, None)])]);
    let annotation_owned = |loaded: &LoadedProviderSnapshot| match annotation(loaded) {
        TypeExpr::SchemaType { owned } => owned,
        _ => panic!("annotation"),
    };
    let a = annotation_owned(&a);
    let b = annotation_owned(&b);
    let c = annotation_owned(&c);
    let ab = a.conjoin(&b);
    let abc = ab.conjoin(&c);
    for joined in [&ab, &abc, &c.conjoin(&ab), &ab.conjoin(&b.conjoin(&c))] {
        let wire = serde_json::to_value(joined).unwrap();
        let restored: crate::parser::ResolvedSchemaType =
            serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(&restored, joined);
        assert_eq!(serde_json::to_value(restored).unwrap(), wire);
        assert_eq!(
            joined.owners().iter().count(),
            wire["constraints"]["owner_layers"]
                .as_object()
                .unwrap()
                .len()
        );
    }
    assert_eq!(abc, c.conjoin(&ab));
    assert_eq!(abc, ab.conjoin(&b.conjoin(&c)));
}
