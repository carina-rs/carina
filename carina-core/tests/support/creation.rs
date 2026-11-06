//! Test fixtures use the real differ-owned finalization boundary, including
//! ancillary effects installed by the finalization callback.

pub fn finalize(plan: carina_core::plan::Plan) -> carina_core::creation::FinalizedPlan {
    use carina_core::{differ::*, provider::*, resource::*, schema::SchemaRegistry};
    use std::collections::{HashMap, HashSet};
    let schemas = SchemaRegistry::new();
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
    .finish(|final_plan| *final_plan = plan)
}

pub fn checked(plan: carina_core::plan::Plan) -> carina_core::creation::CreationCheckedPlan {
    finalize(plan)
        .check_creation(
            &carina_core::schema::SchemaRegistry::new(),
            &std::collections::HashMap::new(),
        )
        .unwrap()
}
