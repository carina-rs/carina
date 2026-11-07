//! Hand-built execution fixtures use the explicitly gated core test API.

pub fn finalize(plan: carina_core::plan::Plan) -> carina_core::input_rules::FinalizedPlan {
    carina_core::input_rules::finalize_plan_for_test(plan)
}

pub fn checked(
    plan: carina_core::plan::Plan,
    schemas: &carina_core::schema::SchemaRegistry,
) -> carina_core::input_rules::InputCheckedPlan {
    finalize(plan)
        .check_inputs(schemas, &std::collections::HashMap::new())
        .unwrap()
}

/// Preserve the coexistence policy of schema-free presentation fixtures.
#[allow(dead_code)] // This shared module is also included by execution fixtures.
pub fn checked_with_derived_schemas(
    plan: carina_core::plan::Plan,
) -> carina_core::input_rules::InputCheckedPlan {
    use carina_core::schema::{PatternResourceSchema, SchemaKind};
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    for effect in plan.effects() {
        let id = effect.resource_id();
        if schemas
            .get(&id.provider, &id.resource_type, SchemaKind::Resource)
            .is_none()
        {
            schemas.insert(
                &id.provider,
                PatternResourceSchema::new(&id.resource_type)
                    .with_coexisting_replacement()
                    .validate()
                    .unwrap(),
            );
        }
    }
    checked(plan, &schemas)
}
