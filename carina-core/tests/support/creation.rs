//! Hand-built execution fixtures use the explicitly gated core test API.

pub fn finalize(plan: carina_core::plan::Plan) -> carina_core::input_rules::FinalizedPlan {
    carina_core::input_rules::finalize_plan_for_test(plan)
}

pub fn checked(plan: carina_core::plan::Plan) -> carina_core::input_rules::InputCheckedPlan {
    let schemas = fixture_schemas(&plan);
    finalize(plan)
        .check_inputs(&schemas, &std::collections::HashMap::new())
        .unwrap()
}

fn fixture_schemas(plan: &carina_core::plan::Plan) -> carina_core::schema::SchemaRegistry {
    use carina_core::{effect::Effect, schema::PatternResourceSchema};
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    for effect in plan.effects() {
        let id = match effect {
            Effect::DeferredCreate(payload) => payload.template.template_resource_id(),
            Effect::DeferredReplace(payload) => payload.template.template_resource_id(),
            _ => effect.resource_id(),
        };
        schemas.insert(
            &id.provider,
            PatternResourceSchema::new(&id.resource_type)
                .with_coexisting_replacement()
                .validate()
                .unwrap(),
        );
    }
    schemas
}
