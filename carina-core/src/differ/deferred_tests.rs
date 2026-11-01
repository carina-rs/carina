use super::*;
use crate::parser::parse;
use crate::resource::ResourceIdentity;
use crate::resource::absolute_binding_address_for_test as absolute_binding;

fn test_identity(value: &str) -> ResourceIdentity {
    ResourceIdentity::new(value)
}
fn fixture_applied_identifier(value: impl Into<String>) -> crate::provider::AppliedIdentifier {
    crate::provider::AppliedIdentifier::new(value).unwrap()
}

fn deferred_replace_test_template() -> crate::parser::DeferredForExpression {
    let template_resource =
        Resource::with_provider("aws", "route53.Record", "validation_records", None)
            .with_absolute_binding(absolute_binding("validation_records"));

    crate::parser::DeferredForExpression::new(
        Some("main.crn".to_string()),
        12,
        "for opt in cert.domain_validation_options".to_string(),
        crate::parser::DeferredForTarget::bound(absolute_binding("validation_records"))
            .expect("fixture binding is valid"),
        Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
        crate::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    )
}

fn deferred_replace_test_target() -> DeferredForExpression {
    deferred_replace_test_template()
}

fn delete_effect_for_binding(binding: &str) -> Effect {
    Effect::Delete {
        id: ResolvedResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            test_identity(binding),
            None,
        ),
        identifier: fixture_applied_identifier(format!("{binding}-old-id")),
        generation: crate::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute_binding(binding)),
        dependencies: crate::test_support::persisted([ResolvedResourceId::with_provider_identity(
            "aws",
            "acm.Certificate",
            "cert",
            None,
        )]),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    }
}

#[test]
fn deferred_create_targets_absorb_matching_orphan_deletes_into_deferred_replace() {
    let target = deferred_replace_test_target();
    let mut plan = Plan::new();
    plan.add(delete_effect_for_binding("validation_records[0]"));

    add_deferred_create_effects(
        &mut plan,
        crate::test_support::resolved_targets(
            DeferredCreateTargets::partition_for_expansion(
                crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test([
                    target.clone()
                ]),
                |_| DeferredForExpansionChoice::Retain,
            )
            .1,
        ),
    );

    let mut deferred_replaces = plan.effects().iter().filter_map(|effect| match effect {
        Effect::DeferredReplace(payload) => {
            Some((&payload.deletes, payload.id(), &payload.template))
        }
        _ => None,
    });
    let (deletes, id, template) = deferred_replaces
        .next()
        .expect("matching delete + deferred target must produce DeferredReplace");
    assert!(
        deferred_replaces.next().is_none(),
        "plan must contain exactly one DeferredReplace"
    );
    assert_eq!(id, target.deferred_effect_id());
    assert_eq!(
        template.root_bindings(),
        std::collections::BTreeSet::from([absolute_binding("cert")])
    );
    assert_eq!(
        template.target().bound_binding(),
        Some("validation_records")
    );
    assert_eq!(deletes.len(), 1);
    assert_eq!(
        deletes[0].id,
        ResourceId::with_provider_identity("aws", "route53.Record", "validation_records[0]", None)
    );
    assert_eq!(
        deletes[0].binding.as_ref().map(|binding| binding.as_str()),
        Some("validation_records[0]")
    );

    assert!(
        !plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    binding: Some(binding),
                    ..
                } if binding.as_str() == "validation_records[0]"
            )
        }),
        "absorbed delete must not remain as a standalone Delete"
    );
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredCreate(_))),
        "absorbed target must not also remain as DeferredCreate"
    );
}

fn unbound_deferred_test_target() -> DeferredForExpression {
    let parsed = parse(
        "let upstream = upstream_state { source = '../upstream' }\n\
         for value in upstream.accounts {\n\
           mock.test.Child { identity_input = value }\n\
         }\n",
        &crate::parser::ProviderContext::default(),
    )
    .expect("unbound deferred loop parses")
    .into_expanded(&crate::schema::SchemaRegistry::new())
    .expect("unbound deferred-loop fixture has no parser placeholders");
    parsed.deferred_for_expressions()[0].clone()
}

fn delete_effect_for_unbound_child(
    identity: &str,
    binding: Option<&str>,
) -> (crate::resource::ResolvedResourceId, Effect) {
    let id = ResolvedResourceId::with_provider_identity(
        "mock",
        "test.Child",
        crate::resource::ResourceIdentity::try_from(identity.to_string())
            .expect("test identity is non-empty"),
        None,
    );
    let effect = Effect::Delete {
        id: id.clone(),
        identifier: fixture_applied_identifier("old-child"),
        generation: crate::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: binding.map(absolute_binding),
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    };
    (id, effect)
}

#[test]
fn unbound_deferred_create_absorbs_exact_legacy_current_delete() {
    let target = unbound_deferred_test_target();
    let (legacy_id, legacy_delete) =
        delete_effect_for_unbound_child("_accounts.alpha", Some("_accounts.alpha"));
    let mut plan = Plan::new();
    plan.add(legacy_delete);

    add_deferred_create_effects(
        &mut plan,
        crate::test_support::resolved_targets(
            DeferredCreateTargets::partition_for_expansion(
                crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test([target]),
                |_| DeferredForExpansionChoice::Retain,
            )
            .1,
        ),
    );

    let payload = plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::DeferredReplace(payload) => Some(payload),
            _ => None,
        })
        .expect("the exact legacy row must be owned by DeferredReplace");
    assert_eq!(payload.deletes.len(), 1);
    assert_eq!(payload.deletes[0].id, legacy_id);
    assert_eq!(
        payload.deletes[0]
            .binding
            .as_ref()
            .map(|binding| binding.as_str()),
        Some("_accounts.alpha")
    );
    assert!(
        plan.effects()
            .iter()
            .all(|effect| !matches!(effect, Effect::Delete { .. })),
        "the legacy delete must not race the runtime recreate",
    );
}

#[test]
fn unbound_deferred_create_keeps_unrelated_anonymous_delete_plain() {
    let target = unbound_deferred_test_target();
    let anonymous_id = ResolvedResourceId::with_provider_identity(
        "mock",
        "test.Child",
        "mock_test_child_deadbeef",
        None,
    );
    let (_, anonymous_delete) = delete_effect_for_unbound_child("mock_test_child_deadbeef", None);
    let mut plan = Plan::new();
    plan.add(anonymous_delete);

    add_deferred_create_effects(
        &mut plan,
        crate::test_support::resolved_targets(
            DeferredCreateTargets::partition_for_expansion(
                crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test([target]),
                |_| DeferredForExpansionChoice::Retain,
            )
            .1,
        ),
    );

    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredCreate(_))),
        "an unrelated anonymous row does not belong to this loop",
    );
    assert!(
        plan
        .effects()
        .iter()
            .any(|effect| matches!(effect, Effect::Delete { id, binding: None, .. } if id == &anonymous_id)),
        "the unrelated anonymous row must remain a standalone Delete",
    );
    assert!(
        plan.effects()
            .iter()
            .all(|effect| !matches!(effect, Effect::DeferredReplace(_))),
        "an unrelated anonymous row must not be absorbed",
    );
}

#[test]
fn deferred_create_targets_absorb_matching_map_key_orphan_deletes() {
    let target = deferred_replace_test_target();
    let mut plan = Plan::new();
    plan.add(delete_effect_for_binding("validation_records.alpha"));
    plan.add(delete_effect_for_binding(
        "validation_records['key with space']",
    ));
    plan.add(delete_effect_for_binding("validation_records['']"));

    add_deferred_create_effects(
        &mut plan,
        crate::test_support::resolved_targets(
            DeferredCreateTargets::partition_for_expansion(
                crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test([
                    target.clone()
                ]),
                |_| DeferredForExpansionChoice::Retain,
            )
            .1,
        ),
    );

    let payload = plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::DeferredReplace(payload) => Some(payload),
            _ => None,
        })
        .expect("map-key deletes must be paired with their deferred create");
    assert_eq!(payload.deletes.len(), 3);
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Delete { .. })),
        "stable map-key children must not remain standalone cleanups beside runtime upserts",
    );
}

#[test]
fn deferred_create_targets_do_not_absorb_deposed_deletes() {
    let target = deferred_replace_test_target();
    let deposed_key = crate::resource::DeposedKey::new_unique();
    let mut deposed_delete = delete_effect_for_binding("validation_records[0]");
    if let Effect::Delete {
        generation,
        identifier,
        ..
    } = &mut deposed_delete
    {
        *generation = crate::effect::EffectGeneration::Deposed(deposed_key.clone());
        *identifier = fixture_applied_identifier("deposed-old-id");
    } else {
        unreachable!("helper builds Delete");
    }
    let mut plan = Plan::new();
    plan.add(deposed_delete);

    add_deferred_create_effects(
        &mut plan,
        crate::test_support::resolved_targets(
            DeferredCreateTargets::partition_for_expansion(
                crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test([
                    target.clone()
                ]),
                |_| DeferredForExpansionChoice::Retain,
            )
            .1,
        ),
    );

    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredCreate(_))),
        "deposed delete must not turn the deferred create into DeferredReplace"
    );
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredReplace(_))),
        "deposed delete must not be absorbed into DeferredReplace"
    );
    assert!(
        plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    binding: Some(binding),
                    generation: crate::effect::EffectGeneration::Deposed(key),
                    ..
                } if binding.as_str() == "validation_records[0]" && key == &deposed_key
            )
        }),
        "deposed delete must remain top-level so remove-deposed writeback can observe it"
    );
}

#[test]
fn deferred_create_targets_do_not_absorb_unrelated_orphan_deletes() {
    let target = deferred_replace_test_target();
    let mut plan = Plan::new();
    plan.add(delete_effect_for_binding("some_other_resource[0]"));

    add_deferred_create_effects(
        &mut plan,
        crate::test_support::resolved_targets(
            DeferredCreateTargets::partition_for_expansion(
                crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test([
                    target.clone()
                ]),
                |_| DeferredForExpansionChoice::Retain,
            )
            .1,
        ),
    );

    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredCreate(_))),
        "unmatched target must stay a plain DeferredCreate"
    );
    assert!(
        plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    binding: Some(binding),
                    ..
                } if binding.as_str() == "some_other_resource[0]"
            )
        }),
        "unrelated orphan delete must stay a plain Delete"
    );
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredReplace(_))),
        "unrelated delete must not be falsely absorbed"
    );
}
