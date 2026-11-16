//! Previously: a display plan's public getter exposed raw observation history.
use super::*;

// Fix 5 RED/PIN: the public display path must not expose divergent history.
#[test]
fn observed_secret_public_display_rejects_kind_divergence() {
    use crate::resource::{ExpandedResource, ExpandedValue, ResolvedResource, SnapshotState};
    let desired: ExpandedResource = ExpandedResource::new("test.Item", "item").with_attribute(
        "payload",
        ExpandedValue::Concrete(ConcreteValue::Map(IndexMap::from([(
            "original".into(),
            ExpandedValue::Concrete(ConcreteValue::String("authored".into())).into_secret(),
        )]))),
    );
    let observed = SnapshotState::existing(
        desired.id.clone(),
        HashMap::from([(
            "payload".into(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("x".into()),
            )])),
        )]),
    );
    let mut plan: Plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("item"),
        from: Box::new(observed),
        to: ResolvedResource::new(desired.with_dependency_phase(())),
        changed_attributes: vec!["payload".into()],
    });
    let plan = plan.admit(&SchemaRegistry::new()).unwrap();
    let historical = HistoricalDisplayAttributes::empty();
    let display = PlanDisplay::unprepared(&plan, &historical, DetailComparisons::RecordedEffects);
    let Effect::Update { from, .. } = &display.plan().effects()[0] else {
        panic!("expected update")
    };
    assert!(
        from.attributes.is_empty(),
        "history requires an owner product"
    );
    assert!(
        from.attributes
            .get("payload")
            .is_none_or(|value| !render_value(value).plain().contains("'x'")),
        "public plan getter exposed divergent observed history: {from:?}"
    );
    let (schemas, provider) = crate::provider::comparison_fixture_for_test(
        &SchemaRegistry::new(),
        std::iter::once(&from.id),
    );
    let rows = display
        .detail_rows(0, DetailLevel::Full, Some(&schemas), None, &provider)
        .unwrap();
    assert!(
        !format!("{rows:?}").contains("'x'"),
        "public detail rows exposed divergent observed history: {rows:?}"
    );
    let observations = crate::value::ObservationState::empty_for_test()
        .protect(crate::value::ObservationInputs::Display {
            deleted: &std::collections::HashMap::new(),
            plan: &plan,
            current: &HashMap::new(),
        })
        .unwrap();
    let historical = HistoricalDisplayAttributes::from_observations(&plan, &observations).unwrap();
    let display = PlanDisplay::unprepared(&plan, &historical, DetailComparisons::RecordedEffects);
    let rows = display
        .detail_rows(0, DetailLevel::Full, Some(&schemas), None, &provider)
        .unwrap();
    assert!(!format!("{rows:?}").contains("'x'"));
    assert!(format!("{rows:?}").contains("(secret)"));
}

#[test]
fn observed_secret_display_rejects_a_batch_owned_by_another_plan() {
    let plan: Plan = Plan::new();
    let other: Plan = Plan::new();
    let observations = crate::value::ObservationState::empty_for_test()
        .protect(crate::value::ObservationInputs::Display {
            deleted: &std::collections::HashMap::new(),
            plan: &other,
            current: &HashMap::new(),
        })
        .unwrap();
    assert!(matches!(
        HistoricalDisplayAttributes::from_observations(&plan, &observations),
        Err(crate::value::SerializationError::ObservationPlanMismatch)
    ));
}

#[test]
fn observed_secret_display_plan_getter_protects_update_history() {
    use crate::resource::{ExpandedResource, ExpandedValue, ResolvedResource, SnapshotState};
    let desired: ExpandedResource = ExpandedResource::new("test.Item", "item").with_attribute(
        "payload",
        ExpandedValue::Concrete(ConcreteValue::String("authored".into())).into_secret(),
    );
    let observed = SnapshotState::existing(
        desired.id.clone(),
        HashMap::from([(
            "payload".into(),
            Value::Concrete(ConcreteValue::String("different-observed".into())),
        )]),
    );
    let mut plan: Plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("item"),
        from: Box::new(observed),
        to: ResolvedResource::new(desired.with_dependency_phase(())),
        changed_attributes: vec!["payload".into()],
    });
    let plan = plan.admit(&SchemaRegistry::new()).unwrap();
    let historical = HistoricalDisplayAttributes::for_test_plan(&plan);
    let display = PlanDisplay::unprepared(&plan, &historical, DetailComparisons::RecordedEffects);
    let Effect::Update { from, .. } = &display.plan().effects()[0] else {
        panic!("expected update")
    };
    assert_eq!(
        render_value(&from.attributes["payload"]).plain(),
        "(secret)"
    );
}

// R4 RED: public delete refinement rejects another resource without a panic.
#[test]
fn observed_secret_delete_identity_mismatch_returns_error() {
    let plan: Plan = Plan::new();
    let mut display = HistoricalDisplayAttributes::for_test_plan(&plan);
    let key = DeletedInstanceKey::new(
        ResourceId::with_identity("test.Item", "item"),
        EffectGeneration::Current,
        "item-id",
    );
    let other = crate::resource::SnapshotState::existing(
        ResourceId::with_identity("test.Item", "other"),
        HashMap::new(),
    );
    let observation = crate::value::ProtectedObservation::classified(&other);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        display.set_delete_from_observation(key, &observation)
    }));
    assert!(result.is_ok(), "identity mismatch must be a typed error");
    assert!(format!("{:?}", result.unwrap()).contains("Err"));
}

// R7 PIN: replacement sub-rows keep the same change decisions as Update.
#[test]
fn observed_secret_nested_replace_rows_keep_changed_and_unchanged_marks() {
    let string = |text: &str| Value::Concrete(ConcreteValue::String(text.into()));
    let map = |entries: Vec<(&str, Value)>| {
        Value::Concrete(ConcreteValue::Map(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        ))
    };
    let desired: crate::resource::Resource<
        crate::resource::ModuleSource,
        crate::resource::SnapshotLeaf,
    > = crate::resource::Resource::new("test.Item", "item")
        .with_attribute(
            "config",
            map(vec![
                (
                    "payload",
                    map(vec![("original", string("authored-private").into_secret())]),
                ),
                ("env", string("new-public")),
                ("stable", string("unchanged-nested")),
            ]),
        )
        .with_attribute("stable", string("unchanged-top-level"));
    let raw = HashMap::from([
        (
            "config".into(),
            map(vec![
                (
                    "payload",
                    Value::Concrete(ConcreteValue::List(vec![string("observed-private")])),
                ),
                ("env", string("old-public")),
                ("stable", string("unchanged-nested")),
            ]),
        ),
        ("stable".into(), string("unchanged-top-level")),
    ]);
    let historical = crate::value::ProtectedObservation::from_authority(
        &crate::resource::SnapshotState::existing(desired.id.clone(), raw.clone()),
        Some(&crate::value::SecretProtection::attributes(
            &desired.attributes,
        )),
        None,
    )
    .display_attributes()
    .clone();
    let current = CanonicalComparisonAttributes::from_unschematized(
        crate::resource::UnschematizedComparisonAttributes::new(raw),
    );
    let to = CanonicalComparisonAttributes::from_unschematized(
        crate::resource::UnschematizedComparisonAttributes::new(desired.resolved_attributes()),
    );
    let rows = crate::differ::exact_comparison_schema_for_tests()
        .with_references(
            &crate::references::prepare_empty_for_tests(),
            &desired.id,
            |comparison| {
                build_replace_rows_from_attributes(
                    &historical,
                    &current,
                    None,
                    &NestedWriteOnlyRecord::new(),
                    &desired,
                    &to,
                    &["config".into()],
                    None,
                    &[],
                    None,
                    DetailLevel::Full,
                    None,
                    *comparison,
                )
            },
        )
        .unwrap();
    let [
        DetailRow::MapDiffForcesReplacement { key, entries },
        DetailRow::HiddenUnchanged { count: 1 },
    ] = rows.as_slice()
    else {
        panic!("replacement must retain changed rows and unchanged count: {rows:?}")
    };
    assert_eq!(key, "config");
    let [
        MapDiffEntryIR::Changed {
            key: env, old, new, ..
        },
        MapDiffEntryIR::Changed {
            key: payload,
            old: hidden_old,
            new: hidden_new,
            ..
        },
    ] = entries.as_slice()
    else {
        panic!("only divergent payload and public env change: {entries:?}")
    };
    assert_eq!(env, &RenderedKey::from_raw("env"));
    assert_eq!(old.plain(), "'old-public'");
    assert_eq!(new.plain(), "'new-public'");
    assert_eq!(payload, &RenderedKey::from_raw("payload"));
    assert_eq!(hidden_old.plain(), "(secret)");
    assert_eq!(hidden_new.plain(), "{original: (secret)}");
    assert!(!format!("{rows:?}").contains("observed-private"));
    assert!(!format!("{rows:?}").contains("authored-private"));
}
