//! Test fixtures use the real differ-owned finalization boundary, including
//! ancillary effects installed by the finalization callback.

pub fn finalize(plan: carina_core::plan::Plan) -> carina_core::input_rules::FinalizedPlan {
    use carina_core::{differ::*, provider::*, resource::*};
    use std::collections::{HashMap, HashSet};
    let schemas = fixture_schemas(&plan);
    let desired = plan_input_desired_resources(&[], &schemas);
    create_plan_with_cascades(
        &desired,
        CascadeDependents::new(&[], ResolvedDeferredCreateTargets::default()),
        &[],
        &ProviderRouter::new(),
        PlanPriorStates::new(&HashMap::new(), &Orphans::new()),
        &HashMap::new(),
        &schemas,
        &RawSavedAttrs::default().lift(&schemas),
        &HashMap::new(),
        &[],
        &HashSet::new(),
        &carina_core::references::prepare_empty_for_tests(),
    )
    .unwrap()
    .finish(std::iter::empty(), |final_plan| *final_plan = plan)
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
